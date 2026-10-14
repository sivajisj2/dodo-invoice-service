use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub app_address: String,
    pub mock_psp_address: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let app_host = env::var("APP_HOST").unwrap_or_else(|_| "127.0.0.1".into());
        let app_port = env::var("APP_PORT")
            .unwrap_or_else(|_| "8080".into())
            .parse::<u16>()?;
        let mock_psp_host = env::var("MOCK_PSP_HOST").unwrap_or_else(|_| "127.0.0.1".into());
        let mock_psp_port = env::var("MOCK_PSP_PORT")
            .unwrap_or_else(|_| "9000".into())
            .parse::<u16>()?;

        Ok(Self {
            app_address: format!("{}:{}", app_host, app_port),
            mock_psp_address: format!("{}:{}", mock_psp_host, mock_psp_port),
        })
    }
}
