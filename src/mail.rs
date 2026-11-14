//! Sending e-mail: login links, password resets and other messages to account holders.
//!
//! [`Mailer`] is the seam: SMTP is built in ([`SmtpMailer`]), [`LogMailer`] writes messages
//! to the log for development, and anything else (an HTTP API such as SendGrid or Postmark)
//! is a few lines implementing the trait.

use async_trait::async_trait;
use lettre::message::{Mailbox, MultiPart};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::error::AdminError;

/// One message to one person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Email {
    /// The recipient's address.
    pub to: String,
    /// The subject line.
    pub subject: String,
    /// The message as plain text.
    pub text: String,
    /// The same message as HTML, for mail programs that show it.
    pub html: Option<String>,
}

/// Something that delivers [`Email`]s.
#[async_trait]
pub trait Mailer: Send + Sync {
    /// Sends one message.
    ///
    /// # Errors
    /// Whatever delivery fails with. The person who caused the message is not told, so that
    /// the answer cannot reveal whether an account exists; the failure is logged.
    async fn send(&self, email: Email) -> Result<(), AdminError>;
}

/// Writes each message to the log instead of sending it: for development, where the login
/// links can be copied from the terminal. This is what a site without `mail.url` uses.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogMailer;

#[async_trait]
impl Mailer for LogMailer {
    async fn send(&self, email: Email) -> Result<(), AdminError> {
        tracing::info!(
            to = %email.to,
            subject = %email.subject,
            "e-mail not sent (no mail.url set); its text:\n{}",
            email.text
        );
        Ok(())
    }
}

/// Sends messages through an SMTP server.
#[derive(Clone)]
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl std::fmt::Debug for SmtpMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpMailer")
            .field("from", &self.from.to_string())
            .finish_non_exhaustive()
    }
}

impl SmtpMailer {
    /// A mailer for the server at `url`, sending as `from` (e.g. `Shop <noreply@shop.com>`).
    ///
    /// | URL | Connection |
    /// |---|---|
    /// | `smtps://user:password@smtp.example.com` | TLS from the start, port 465 (preferred) |
    /// | `smtp://user:password@smtp.example.com?tls=required` | STARTTLS, port 587 |
    /// | `smtp://localhost:1025` | unencrypted: only to this machine, e.g. a test inbox such as Mailpit |
    ///
    /// Characters such as `@` or `/` in the user name or password must be percent-encoded.
    ///
    /// # Errors
    /// Returns [`AdminError::Config`] if the URL or the sender is malformed, or the URL would
    /// send passwords and messages unencrypted to another machine.
    pub fn new(url: &str, from: &str) -> Result<Self, AdminError> {
        let parsed =
            url::Url::parse(url).map_err(|_| AdminError::Config("mail.url is not a URL".into()))?;
        let local = matches!(
            parsed.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        );
        let tls = parsed
            .query_pairs()
            .find(|(key, _)| key == "tls")
            .map(|(_, value)| value.into_owned());
        let encrypted = match parsed.scheme() {
            "smtps" => true,
            "smtp" => tls.as_deref() == Some("required"),
            other => {
                return Err(AdminError::Config(format!(
                    "mail.url must start with smtps:// or smtp://, not {other}://"
                )));
            }
        };
        if !encrypted && !local {
            return Err(AdminError::Config(
                "mail.url would send unencrypted: use smtps://… or smtp://…?tls=required".into(),
            ));
        }
        let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(url)
            .map_err(|e| AdminError::Config(format!("mail.url: {e}")))?
            .build();
        let from = from
            .parse::<Mailbox>()
            .map_err(|e| AdminError::Config(format!("mail.from `{from}`: {e}")))?;
        Ok(Self { transport, from })
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, email: Email) -> Result<(), AdminError> {
        let to = email
            .to
            .parse::<Mailbox>()
            .map_err(|e| AdminError::Custom {
                status: axum::http::StatusCode::BAD_GATEWAY,
                code: "mail_failed".into(),
                message: format!("cannot send to `{}`: {e}", email.to),
            })?;
        let builder = Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(email.subject);
        let message = match email.html {
            Some(html) => builder.multipart(MultiPart::alternative_plain_html(email.text, html)),
            None => builder.body(email.text),
        }
        .map_err(|e| mail_failed(&e))?;
        self.transport
            .send(message)
            .await
            .map_err(|e| mail_failed(&e))?;
        Ok(())
    }
}

fn mail_failed(error: &dyn std::fmt::Display) -> AdminError {
    AdminError::Custom {
        status: axum::http::StatusCode::BAD_GATEWAY,
        code: "mail_failed".into(),
        message: format!("the e-mail could not be sent: {error}"),
    }
}

/// Sends `email` without making the request wait, logging a failure. The reply to whoever
/// asked must not depend on whether a message was sent, or it would tell them whether the
/// address has an account.
pub(crate) fn send_later(mailer: std::sync::Arc<dyn Mailer>, email: Email) {
    tokio::spawn(async move {
        let to = email.to.clone();
        if let Err(error) = mailer.send(email).await {
            tracing::error!(%to, %error, "e-mail could not be sent");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // The connection pool needs a running Tokio runtime, as it has inside `serve`.
    #[tokio::test]
    async fn unencrypted_smtp_is_only_allowed_locally() {
        assert!(SmtpMailer::new("smtps://u:p@smtp.example.com", "Shop <a@shop.com>").is_ok());
        assert!(SmtpMailer::new("smtp://u:p@smtp.example.com?tls=required", "a@shop.com").is_ok());
        assert!(SmtpMailer::new("smtp://localhost:1025", "a@shop.com").is_ok());
        for url in [
            "smtp://u:p@smtp.example.com",
            "smtp://u:p@smtp.example.com?tls=opportunistic",
        ] {
            match SmtpMailer::new(url, "a@shop.com") {
                Err(AdminError::Config(message)) => assert!(message.contains("unencrypted")),
                _ => panic!("{url} must be refused"),
            }
        }
        assert!(SmtpMailer::new("http://smtp.example.com", "a@shop.com").is_err());
        assert!(SmtpMailer::new("smtps://smtp.example.com", "not an address").is_err());
    }
}
