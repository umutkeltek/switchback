use std::io::Read;
use std::path::Path;

use clap::Subcommand;
use sb_core::Config;

#[derive(Subcommand)]
pub(crate) enum VaultCmd {
    /// Generate a key (stored in the OS keychain) and create an empty vault file.
    Init,
    /// Print a fresh age key for SWITCHBACK_VAULT_KEY (headless / CI / no keychain).
    Keygen,
    /// Add or replace a secret. Value from --value, else read from stdin.
    Set {
        name: String,
        #[arg(long)]
        value: Option<String>,
    },
    /// List secret names (never values).
    List,
    /// Remove a secret.
    Rm { name: String },
}

pub(crate) fn run_vault_cmd(action: VaultCmd, config: &Path, json: bool) -> anyhow::Result<()> {
    // Keygen needs no config/vault section — it just mints a key.
    if let VaultCmd::Keygen = action {
        let key = sb_credentials::vault::generate_identity_string();
        if json {
            crate::print_json(&serde_json::json!({ "key": key }))?;
        } else {
            println!("{key}");
        }
        return Ok(());
    }

    let cfg = Config::from_path(config)?;
    let vc = cfg.vault.ok_or_else(|| {
        anyhow::anyhow!(
            "no `vault:` section in {} — add one (path + keychain_service)",
            config.display()
        )
    })?;
    let path = std::path::Path::new(&vc.path);
    let service = &vc.keychain_service;
    match action {
        VaultCmd::Keygen => unreachable!("handled above"),
        VaultCmd::Init => {
            sb_credentials::vault::init(path, service).map_err(|e| anyhow::anyhow!(e))?;
            if json {
                crate::print_json(&serde_json::json!({ "ok": true, "vault": vc.path }))?;
            } else {
                println!("vault initialized at {}", vc.path);
            }
        }
        VaultCmd::Set { name, value } => {
            let value = match value {
                Some(value) => value,
                None => {
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    buf.trim_end_matches(['\n', '\r']).to_string()
                }
            };
            sb_credentials::vault::set_secret(path, service, &name, &value)
                .map_err(|e| anyhow::anyhow!(e))?;
            if json {
                crate::print_json(&serde_json::json!({ "ok": true, "name": name }))?;
            } else {
                println!("set secret `{name}`");
            }
        }
        VaultCmd::List => {
            let names = sb_credentials::vault::list_secrets(path, service)
                .map_err(|e| anyhow::anyhow!(e))?;
            if json {
                crate::print_json(&serde_json::json!({ "secrets": names }))?;
            } else {
                if names.is_empty() {
                    println!("(vault is empty)");
                }
                for name in names {
                    println!("{name}");
                }
            }
        }
        VaultCmd::Rm { name } => {
            let removed = sb_credentials::vault::remove_secret(path, service, &name)
                .map_err(|e| anyhow::anyhow!(e))?;
            if json {
                crate::print_json(
                    &serde_json::json!({ "ok": true, "name": name, "removed": removed }),
                )?;
            } else {
                println!(
                    "{}",
                    if removed {
                        format!("removed `{name}`")
                    } else {
                        format!("`{name}` not found")
                    }
                );
            }
        }
    }
    Ok(())
}
