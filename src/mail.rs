//! Sending e-mail: login links, password resets and other messages to account holders.
//!
//! [`Mailer`] is the seam: SMTP comes with the `smtp` feature (`SmtpMailer`), [`LogMailer`] writes messages
//! to the log for development, and anything else (an HTTP API such as SendGrid or Postmark)
//! is a few lines implementing the trait.

use async_trait::async_trait;

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

#[cfg(feature = "smtp")]
mod smtp;
#[cfg(feature = "smtp")]
pub use smtp::SmtpMailer;
