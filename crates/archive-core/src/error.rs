use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("unsupported or unrecognised archive format")]
    UnsupportedFormat,

    #[error("a password is required to open this archive")]
    PasswordRequired,

    #[error("the password is incorrect")]
    WrongPassword,

    #[error("archive is corrupt: {0}")]
    Corrupt(String),

    #[error("entry path is not safe to extract: {}", .0.display())]
    UnsafePath(PathBuf),

    #[error("entry not found in archive: {}", .0.display())]
    NotFound(PathBuf),

    #[error("operation cancelled")]
    Cancelled,

    #[error("this archive format cannot be written: {0:?}")]
    ReadOnlyFormat(crate::Format),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<zip::result::ZipError> for Error {
    fn from(e: zip::result::ZipError) -> Self {
        use zip::result::ZipError as Z;
        match e {
            Z::Io(e) => Error::Io(e),
            Z::InvalidPassword => Error::WrongPassword,
            Z::UnsupportedArchive(msg) if msg.contains("assword") => Error::PasswordRequired,
            Z::UnsupportedArchive(msg) => Error::Corrupt(format!("unsupported zip feature: {msg}")),
            Z::FileNotFound => Error::NotFound(PathBuf::new()),
            Z::InvalidArchive(msg) if msg.starts_with("Duplicate filename") => {
                Error::Corrupt("the same entry name occurs twice".into())
            }
            Z::InvalidArchive(msg) => Error::Corrupt(msg.into_owned()),
            other => Error::Corrupt(other.to_string()),
        }
    }
}

impl From<sevenz_rust2::Error> for Error {
    fn from(e: sevenz_rust2::Error) -> Self {
        use sevenz_rust2::Error as S;
        match e {
            S::Io(e, _) => Error::Io(e),
            S::FileOpen(e, _) => Error::Io(e),
            S::PasswordRequired => Error::PasswordRequired,
            S::MaybeBadPassword(_) => Error::WrongPassword,
            S::BadSignature(_) => Error::UnsupportedFormat,
            S::FileNotFound => Error::NotFound(PathBuf::new()),
            other => Error::Corrupt(other.to_string()),
        }
    }
}
