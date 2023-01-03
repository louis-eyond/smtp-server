use std::{
    borrow::Cow,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use mail_send::{Credentials, SmtpClient};
use rand::{seq::SliceRandom, Rng};
use smtp_proto::{Severity, MAIL_REQUIRETLS};

use crate::{
    config::{RelayHost, ServerProtocol},
    core::Core,
};

use super::{
    manager::Queue,
    session::{into_tls, read_greeting, say_helo, try_start_tls, SessionParams, StartTlsResult},
    throttle, DeliveryAttempt, Domain, DomainStatus, Error, Event, Message, OnHold, QueueEnvelope,
    Schedule, WorkerResult,
};

impl DeliveryAttempt {
    pub async fn try_deliver(mut self, core: Arc<Core>, queue: &mut Queue) {
        // Send any due Delivery Status Notifications
        core.queue.send_dsn(&mut self.message).await;

        // Throttle sender
        for throttle in &core.queue.config.throttle.sender {
            if let Err(err) = core
                .queue
                .is_allowed(
                    throttle,
                    self.message.as_ref(),
                    &mut self.in_flight,
                    &self.span,
                )
                .await
            {
                match err {
                    throttle::Error::Concurrency { limiter } => {
                        queue.on_hold.push(OnHold {
                            next_due: self.message.next_event_after(Instant::now()),
                            limiters: vec![limiter],
                            message: self.message,
                        });
                    }
                    throttle::Error::Rate { retry_at } => {
                        queue.main.push(Schedule {
                            due: retry_at,
                            inner: self.message,
                        });
                    }
                }
                return;
            }
        }

        let coco = "add logging";

        tokio::spawn(async move {
            let queue_config = &core.queue.config;
            let mut on_hold = Vec::new();
            let no_ip = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));

            let mut domains = std::mem::take(&mut self.message.domains);
            let mut recipients = std::mem::take(&mut self.message.recipients);
            'next_domain: for (domain_idx, domain) in domains.iter_mut().enumerate() {
                // Only process domains due for delivery
                if !matches!(&domain.status, DomainStatus::Scheduled | DomainStatus::TemporaryFailure(_)
                if domain.retry.due <= Instant::now())
                {
                    continue;
                }

                // Create new span
                let span = tracing::info_span!(
                    parent: &self.span,
                    "attempt",
                    "domain" = domain.domain,
                );

                // Build envelope
                let mut envelope = QueueEnvelope {
                    message: self.message.as_ref(),
                    domain: &domain.domain,
                    mx: "",
                    remote_ip: no_ip,
                    local_ip: no_ip,
                };

                // Throttle recipient domain
                let mut in_flight = Vec::new();
                for throttle in &queue_config.throttle.rcpt {
                    if let Err(err) = core
                        .queue
                        .is_allowed(throttle, &envelope, &mut in_flight, &span)
                        .await
                    {
                        domain.set_throttle_error(err, &mut on_hold);
                        continue 'next_domain;
                    }
                }

                // Obtain remote hosts list
                let mx_list;
                let remote_hosts =
                    if let Some(next_hop) = queue_config.next_hop.eval(&envelope).await {
                        vec![RemoteHost::Relay(next_hop)]
                    } else {
                        // Lookup MX
                        mx_list = match core.resolvers.dns.mx_lookup(&domain.domain).await {
                            Ok(mx) => mx,
                            Err(err) => {
                                domain.set_status(err, queue_config.retry.eval(&envelope).await);
                                continue 'next_domain;
                            }
                        };

                        if !mx_list.is_empty() {
                            // Obtain max number of MX hosts to process
                            let max_mx = *queue_config.max_mx.eval(&envelope).await;
                            let mut remote_hosts = Vec::with_capacity(max_mx);

                            for mx in mx_list.iter() {
                                if mx.exchanges.len() > 1 {
                                    let mut slice = mx.exchanges.iter().collect::<Vec<_>>();
                                    slice.shuffle(&mut rand::thread_rng());
                                    for remote_host in slice {
                                        remote_hosts.push(RemoteHost::MX(remote_host.as_str()));
                                        if remote_hosts.len() == max_mx {
                                            break;
                                        }
                                    }
                                } else if let Some(remote_host) = mx.exchanges.first() {
                                    remote_hosts.push(RemoteHost::MX(remote_host.as_str()));
                                    if remote_hosts.len() == max_mx {
                                        break;
                                    }
                                }
                            }
                            remote_hosts
                        } else {
                            // If an empty list of MXs is returned, the address is treated as if it was
                            // associated with an implicit MX RR with a preference of 0, pointing to that host.
                            vec![RemoteHost::MX(domain.domain.as_str())]
                        }
                    };

                // Try delivering message
                let max_multihomed = *queue_config.max_multihomed.eval(&envelope).await;
                let mut last_status = DomainStatus::Scheduled;
                'next_host: for remote_host in &remote_hosts {
                    // Obtain source and remote IPs
                    envelope.mx = remote_host.hostname();
                    let (source_ip, remote_ips) = match core
                        .resolve_host(remote_host, &envelope, max_multihomed)
                        .await
                    {
                        Ok(result) => result,
                        Err(status) => {
                            last_status = status;
                            continue 'next_host;
                        }
                    };

                    // Try each IP address
                    envelope.local_ip = source_ip.unwrap_or(no_ip);
                    'next_ip: for remote_ip in remote_ips {
                        // Throttle remote host
                        let mut in_flight_host = Vec::new();
                        envelope.remote_ip = remote_ip;
                        for throttle in &queue_config.throttle.host {
                            if let Err(err) = core
                                .queue
                                .is_allowed(throttle, &envelope, &mut in_flight_host, &span)
                                .await
                            {
                                domain.set_throttle_error(err, &mut on_hold);
                                continue 'next_domain;
                            }
                        }

                        // Connect
                        let mut smtp_client = match if let Some(ip_addr) = source_ip {
                            SmtpClient::connect_using(
                                ip_addr,
                                SocketAddr::new(remote_ip, remote_host.port()),
                                *queue_config.timeout_connect.eval(&envelope).await,
                            )
                            .await
                        } else {
                            SmtpClient::connect(
                                SocketAddr::new(remote_ip, remote_host.port()),
                                *queue_config.timeout_connect.eval(&envelope).await,
                            )
                            .await
                        } {
                            Ok(smtp_client) => smtp_client,
                            Err(err) => {
                                last_status =
                                    DomainStatus::from(("Failed to connect to", envelope.mx, err));
                                continue 'next_ip;
                            }
                        };

                        // Obtain TLS strategy
                        let tls_strategy = *queue_config.encryption.eval(&envelope).await;
                        let tls_connector = if !remote_host.allow_invalid_certs() {
                            &core.queue.connectors.pki_verify
                        } else {
                            &core.queue.connectors.dummy_verify
                        };

                        // Obtail session parameters
                        let params = SessionParams {
                            span: &span,
                            hostname: envelope.mx,
                            credentials: remote_host.credentials(),
                            is_smtp: remote_host.is_smtp(),
                            ehlo_hostname: queue_config.ehlo_name.eval(&envelope).await,
                            timeout_ehlo: *queue_config.timeout_ehlo.eval(&envelope).await,
                            timeout_mail: *queue_config.timeout_mail.eval(&envelope).await,
                            timeout_rcpt: *queue_config.timeout_rcpt.eval(&envelope).await,
                            timeout_data: *queue_config.timeout_data.eval(&envelope).await,
                        };

                        let delivery_result = if !remote_host.implicit_tls() {
                            // Read greeting
                            smtp_client.timeout =
                                *queue_config.timeout_greeting.eval(&envelope).await;
                            if let Err(status) = read_greeting(&mut smtp_client, envelope.mx).await
                            {
                                last_status = status;
                                continue 'next_host;
                            }

                            // Say EHLO
                            let capabilties = match say_helo(&mut smtp_client, &params).await {
                                Ok(capabilities) => capabilities,
                                Err(status) => {
                                    last_status = status;
                                    continue 'next_host;
                                }
                            };

                            // Try starting TLS
                            smtp_client.timeout = *queue_config.timeout_tls.eval(&envelope).await;
                            match try_start_tls(
                                smtp_client,
                                tls_connector,
                                envelope.mx,
                                &capabilties,
                            )
                            .await
                            {
                                Ok(StartTlsResult::Success { smtp_client }) => {
                                    // Verify DANE
                                    if tls_strategy.is_dane() {
                                        if let Err(status) = core
                                            .resolvers
                                            .dnssec
                                            .verify_dane(
                                                &span,
                                                envelope.mx,
                                                tls_strategy.is_dane_required(),
                                                smtp_client.tls_connection().peer_certificates(),
                                            )
                                            .await
                                        {
                                            last_status = status;
                                            continue 'next_host;
                                        }
                                    }

                                    // Deliver message over TLS
                                    self.message
                                        .deliver(
                                            smtp_client,
                                            recipients
                                                .iter_mut()
                                                .filter(|r| r.domain_idx == domain_idx),
                                            params,
                                        )
                                        .await
                                }
                                Ok(StartTlsResult::Unavailable {
                                    response,
                                    smtp_client,
                                }) => {
                                    if tls_strategy.is_tls_required()
                                        || (self.message.flags & MAIL_REQUIRETLS) != 0
                                    {
                                        last_status = if let Some(response) = response {
                                            DomainStatus::from((
                                                "TLS unavailable for",
                                                envelope.mx,
                                                mail_send::Error::UnexpectedReply(response),
                                            ))
                                        } else {
                                            DomainStatus::PermanentFailure(Error::ConnectionError(
                                                format!(
                                                    "STARTTLS not supported by host {:?}.",
                                                    envelope.mx
                                                ),
                                            ))
                                        };
                                        continue 'next_host;
                                    } else {
                                        // TLS is not required, proceed in plain-text
                                        self.message
                                            .deliver(
                                                smtp_client,
                                                recipients
                                                    .iter_mut()
                                                    .filter(|r| r.domain_idx == domain_idx),
                                                params,
                                            )
                                            .await
                                    }
                                }
                                Err(status) => {
                                    last_status = status;
                                    continue 'next_host;
                                }
                            }
                        } else {
                            // Start TLS
                            smtp_client.timeout = *queue_config.timeout_tls.eval(&envelope).await;
                            let mut smtp_client =
                                match into_tls(smtp_client, tls_connector, envelope.mx).await {
                                    Ok(smtp_client) => smtp_client,
                                    Err(status) => {
                                        last_status = status;
                                        continue 'next_host;
                                    }
                                };

                            // Read greeting
                            smtp_client.timeout =
                                *queue_config.timeout_greeting.eval(&envelope).await;
                            if let Err(status) = read_greeting(&mut smtp_client, envelope.mx).await
                            {
                                last_status = status;
                                continue 'next_host;
                            }

                            // Deliver message
                            self.message
                                .deliver(
                                    smtp_client,
                                    recipients.iter_mut().filter(|r| r.domain_idx == domain_idx),
                                    params,
                                )
                                .await
                        };

                        // Update status for the current domain and continue with the next one
                        domain
                            .set_status(delivery_result, queue_config.retry.eval(&envelope).await);
                        continue 'next_domain;
                    }
                }

                // Update status
                domain.set_status(last_status, queue_config.retry.eval(&envelope).await);
            }
            self.message.domains = domains;
            self.message.recipients = recipients;

            // Send Delivery Status Notifications
            core.queue.send_dsn(&mut self.message).await;

            // Notify queue manager
            let span = self.span;
            let result = if !on_hold.is_empty() {
                // Release quota for completed deliveries
                self.message.release_quota();

                WorkerResult::OnHold(OnHold {
                    next_due: self.message.next_event_after(Instant::now()),
                    limiters: on_hold,
                    message: self.message,
                })
            } else if let Some(due) = self.message.next_event() {
                // Release quota for completed deliveries
                self.message.release_quota();

                WorkerResult::Retry(Schedule {
                    due,
                    inner: self.message,
                })
            } else {
                WorkerResult::Done
            };
            if core.queue.tx.send(Event::Done(result)).await.is_err() {
                tracing::warn!(
                    parent: &span,
                    "Channel closed while trying to notify queue manager."
                );
            }
        });
    }
}

enum RemoteHost<'x> {
    Relay(&'x RelayHost),
    MX(&'x str),
}

impl<'x> RemoteHost<'x> {
    fn hostname(&self) -> &str {
        match self {
            RemoteHost::MX(host) => host,
            RemoteHost::Relay(host) => host.address.as_str(),
        }
    }

    fn fqdn_hostname(&self) -> Cow<'_, str> {
        match self {
            RemoteHost::MX(host) => {
                if !host.ends_with('.') {
                    format!("{}.", host).into()
                } else {
                    (*host).into()
                }
            }
            RemoteHost::Relay(host) => host.address.as_str().into(),
        }
    }

    fn port(&self) -> u16 {
        match self {
            RemoteHost::MX(_) => 25,
            RemoteHost::Relay(host) => host.port,
        }
    }

    fn credentials(&self) -> Option<&Credentials<String>> {
        match self {
            RemoteHost::MX(_) => None,
            RemoteHost::Relay(host) => host.auth.as_ref(),
        }
    }

    fn allow_invalid_certs(&self) -> bool {
        match self {
            RemoteHost::MX(_) => false,
            RemoteHost::Relay(host) => host.tls_allow_invalid_certs,
        }
    }

    fn implicit_tls(&self) -> bool {
        match self {
            RemoteHost::MX(_) => false,
            RemoteHost::Relay(host) => host.tls_implicit,
        }
    }

    fn is_smtp(&self) -> bool {
        match self {
            RemoteHost::MX(_) => true,
            RemoteHost::Relay(host) => host.protocol == ServerProtocol::Smtp,
        }
    }
}

impl Core {
    async fn resolve_host(
        &self,
        remote_host: &RemoteHost<'_>,
        envelope: &QueueEnvelope<'_>,
        max_multihomed: usize,
    ) -> Result<(Option<IpAddr>, Vec<IpAddr>), DomainStatus> {
        let mut remote_ips = Vec::new();
        let mut source_ip = None;

        for (pos, remote_ip) in self
            .resolvers
            .dns
            .ip_lookup(remote_host.fqdn_hostname().as_ref())
            .await?
            .take(max_multihomed)
            .enumerate()
        {
            if pos == 0 {
                if remote_ip.is_ipv4() {
                    let source_ips = self.queue.config.source_ipv4.eval(envelope).await;
                    match source_ips.len().cmp(&1) {
                        std::cmp::Ordering::Equal => {
                            source_ip = IpAddr::from(*source_ips.first().unwrap()).into();
                        }
                        std::cmp::Ordering::Greater => {
                            source_ip = IpAddr::from(
                                source_ips[rand::thread_rng().gen_range(0..source_ips.len())],
                            )
                            .into();
                        }
                        std::cmp::Ordering::Less => (),
                    }
                } else {
                    let source_ips = self.queue.config.source_ipv6.eval(envelope).await;
                    match source_ips.len().cmp(&1) {
                        std::cmp::Ordering::Equal => {
                            source_ip = IpAddr::from(*source_ips.first().unwrap()).into();
                        }
                        std::cmp::Ordering::Greater => {
                            source_ip = IpAddr::from(
                                source_ips[rand::thread_rng().gen_range(0..source_ips.len())],
                            )
                            .into();
                        }
                        std::cmp::Ordering::Less => (),
                    }
                }
            }
            remote_ips.push(remote_ip);
        }

        // Make sure there is at least one IP address
        if !remote_ips.is_empty() {
            Ok((source_ip, remote_ips))
        } else {
            Err(DomainStatus::TemporaryFailure(Error::DnsError(format!(
                "No IP addresses found for {:?}.",
                envelope.mx
            ))))
        }
    }
}

impl Domain {
    pub fn set_status(&mut self, status: impl Into<DomainStatus>, schedule: &[Duration]) {
        self.status = status.into();
        if matches!(&self.status, DomainStatus::TemporaryFailure(_)) {
            self.retry(schedule);
        }
    }

    pub fn retry(&mut self, schedule: &[Duration]) {
        self.retry.due =
            Instant::now() + schedule[std::cmp::min(self.retry.inner as usize, schedule.len() - 1)];
        self.retry.inner += 1;
    }
}

impl From<(&str, &str, mail_send::Error)> for DomainStatus {
    fn from(value: (&str, &str, mail_send::Error)) -> Self {
        match value.2 {
            mail_send::Error::Io(_)
            | mail_send::Error::Base64(_)
            | mail_send::Error::UnparseableReply
            | mail_send::Error::AuthenticationFailed(_)
            | mail_send::Error::MissingCredentials
            | mail_send::Error::MissingMailFrom
            | mail_send::Error::MissingRcptTo
            | mail_send::Error::Timeout => DomainStatus::TemporaryFailure(Error::ConnectionError(
                format!("{} {:?}: {}", value.0, value.1, value.2),
            )),

            mail_send::Error::UnexpectedReply(reply) => {
                let message = format!("{} {:?}", value.0, value.1);
                if reply.severity() == Severity::PermanentNegativeCompletion {
                    DomainStatus::PermanentFailure(Error::UnexpectedResponse {
                        message,
                        response: reply,
                    })
                } else {
                    DomainStatus::TemporaryFailure(Error::UnexpectedResponse {
                        message,
                        response: reply,
                    })
                }
            }

            mail_send::Error::Auth(_)
            | mail_send::Error::UnsupportedAuthMechanism
            | mail_send::Error::InvalidTLSName
            | mail_send::Error::MissingStartTls => DomainStatus::TemporaryFailure(
                Error::ConnectionError(format!("{} {:?}: {}", value.0, value.1, value.2)),
            ),
        }
    }
}

impl From<mail_auth::Error> for DomainStatus {
    fn from(err: mail_auth::Error) -> Self {
        match &err {
            mail_auth::Error::DNSRecordNotFound(code) => DomainStatus::PermanentFailure(
                Error::DnsError(format!("Domain not found: {}", code)),
            ),
            _ => DomainStatus::TemporaryFailure(Error::DnsError(err.to_string())),
        }
    }
}

impl From<Box<Message>> for DeliveryAttempt {
    fn from(message: Box<Message>) -> Self {
        DeliveryAttempt {
            span: tracing::info_span!(
                "delivery",
                "queue-id" = message.id,
                "from" = message.return_path_lcase,
                "size" = message.size,
                "nrcpt" = message.recipients.len()
            ),
            in_flight: Vec::new(),
            message,
        }
    }
}
