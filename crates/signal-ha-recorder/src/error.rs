use thiserror::Error;

#[derive(Debug, Error)]
pub enum RecorderError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[cfg(feature = "sqlite")]
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[cfg(feature = "mysql")]
    #[error("MySQL error: {0}")]
    Mysql(#[from] mysql::Error),

    #[error("Home Assistant error: {0}")]
    Ha(#[from] signal_ha::HaError),

    #[error("{0}")]
    Other(String),
}
