use crate::Error;

/// Stable, privacy-safe diagnostics for persisted operations and the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileIssue {
    Permission,
    Quota,
    Authentication,
    Connection,
    RateLimit,
    LocalStorage,
    Unsupported,
    Other,
}

impl FileIssue {
    pub fn from_error(error: &Error) -> Self {
        match error {
            Error::AuthRequired => Self::Authentication,
            Error::Transport(_) => Self::Connection,
            Error::Io(_) => Self::LocalStorage,
            Error::Unsupported(_) => Self::Unsupported,
            Error::Http { status, reason } => Self::http(*status, reason),
            _ => Self::Other,
        }
    }

    fn http(status: u16, reason: &str) -> Self {
        match reason {
            "storageQuotaExceeded" => Self::Quota,
            "rateLimitExceeded" | "userRateLimitExceeded" => Self::RateLimit,
            "insufficientFilePermissions" => Self::Permission,
            _ => match status {
                401 => Self::Authentication,
                403 => Self::Permission,
                429 => Self::RateLimit,
                _ => Self::Other,
            },
        }
    }

    /// Older journals store only the sanitized Error display string.
    pub fn from_legacy(error: &str) -> Self {
        if error == "authentication required" {
            Self::Authentication
        } else if error == "transport failure" {
            Self::Connection
        } else if let Some(http) = error.strip_prefix("Drive HTTP ") {
            http.split_once(": ")
                .and_then(|(status, reason)| status.parse().ok().map(|s| Self::http(s, reason)))
                .unwrap_or(Self::Other)
        } else if error.starts_with("filesystem: ") {
            Self::LocalStorage
        } else if error.starts_with("unsupported: ") {
            Self::Unsupported
        } else {
            Self::Other
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Permission => crate::i18n::tr("Izin file ditolak. Periksa akses akun ke file di Google Drive; aplikasi akan mencoba lagi."),
            Self::Quota => crate::i18n::tr("Penyimpanan Google Drive penuh. Kosongkan ruang atau tambah kuota; aplikasi akan mencoba lagi."),
            Self::Authentication => crate::i18n::tr("Login Google perlu diperbarui. Buka Akun dan folder → Login ulang akun terpilih."),
            Self::Connection => crate::i18n::tr("Koneksi Drive terputus. Periksa jaringan; aplikasi akan mencoba lagi otomatis."),
            Self::RateLimit => crate::i18n::tr("Drive membatasi permintaan. Tunggu percobaan ulang otomatis."),
            Self::LocalStorage => crate::i18n::tr("File lokal belum dapat dibaca atau ditulis. Periksa ruang disk dan izin folder; aplikasi akan mencoba lagi."),
            Self::Unsupported => crate::i18n::tr("Operasi file belum didukung. Periksa jenis file dan batas dukungan aplikasi."),
            Self::Other => crate::i18n::tr("Operasi gagal. Periksa status file dan diagnostik layanan; aplikasi akan mencoba lagi."),
        }
    }
}

/// Account/network failures stop a cycle; a file permission failure does not.
pub(crate) fn isolated_metadata_error(error: &Error) -> bool {
    matches!(error, Error::Http { status: 403, reason }
        if !matches!(reason.as_str(), "rateLimitExceeded" | "userRateLimitExceeded"))
        || matches!(error, Error::Unsupported(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_errors_show_actions_without_echoing_untrusted_text() {
        for (error, issue) in [
            ("Drive HTTP 403: storageQuotaExceeded", FileIssue::Quota),
            (
                "Drive HTTP 403: insufficientFilePermissions",
                FileIssue::Permission,
            ),
            ("Drive HTTP 429: requestFailed", FileIssue::RateLimit),
            ("authentication required", FileIssue::Authentication),
            ("transport failure", FileIssue::Connection),
            ("filesystem: secret-user-path", FileIssue::LocalStorage),
            ("untrusted-secret", FileIssue::Other),
        ] {
            assert_eq!(FileIssue::from_legacy(error), issue);
            assert!(!issue.description().contains("secret"));
        }
    }

    #[test]
    fn throttling_is_not_mistaken_for_a_file_permission_error() {
        for reason in ["rateLimitExceeded", "userRateLimitExceeded"] {
            let error = Error::Http {
                status: 403,
                reason: reason.into(),
            };
            assert_eq!(FileIssue::from_error(&error), FileIssue::RateLimit);
            assert!(!isolated_metadata_error(&error));
        }
        assert!(!isolated_metadata_error(&Error::AuthRequired));
    }
}
