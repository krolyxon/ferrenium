//! Ready-to-use deployment templates, printed by `ferrenium example`.

pub const CONFIG: &str = include_str!("../dist/ferrenium.toml");
pub const SYSTEMD: &str = include_str!("../dist/ferrenium.service");
pub const CADDY: &str = include_str!("../dist/Caddyfile");
