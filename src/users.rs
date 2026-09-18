//! Headless operator roster: `--add-user` and `--set-password`.
//!
//! A station several people work is edited with a text editor or these two
//! flags. There is no admin dialog on purpose — the roster is a file on the
//! machine the radio is attached to, and hashing a password here is the one
//! thing a text editor cannot do.

use anyhow::{Context, Result, bail};
use sdroxide_types::User;

fn read_password(name: &str) -> Result<String> {
    if let Ok(p) = std::env::var("SDROXIDE_PASSWORD") {
        if !p.is_empty() {
            return Ok(p);
        }
    }
    eprint!("Password for {name}: ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).context("reading the password")?;
    let pass = line.trim_end_matches(['\r', '\n']).to_string();
    if pass.is_empty() {
        bail!("an empty password is not stored — the operator would never get in");
    }
    Ok(pass)
}

fn write_roster(users: &sdroxide_types::Users) -> Result<()> {
    sdroxide_config::save_users(users).context("writing users.toml")?;
    if let Some(path) = sdroxide_config::users_path() {
        eprintln!("wrote {}", path.display());
    }
    Ok(())
}

/// Put `name` on the roster with a freshly hashed password.
pub fn add_user(name: &str) -> Result<()> {
    let Some(safe) = sdroxide_config::sanitize_username(name) else {
        bail!("that name is not a usable operator — use letters, digits and hyphen");
    };
    let mut users = sdroxide_config::load_users();
    if users.find(&safe).is_some() {
        bail!("{safe} is already on the roster; use --set-password to change their password");
    }
    let password = read_password(&safe)?;
    let password_hash = sdroxide_server::hash_password(&password).map_err(|e| anyhow::anyhow!(e))?;
    users.users.push(User {
        name: safe.clone(),
        password_hash,
        password: String::new(),
        may_transmit: true,
    });
    write_roster(&users)?;
    eprintln!("added {safe}");
    Ok(())
}

/// Replace `name`'s password. They stay on the roster with the rights they had.
pub fn set_password(name: &str) -> Result<()> {
    let Some(safe) = sdroxide_config::sanitize_username(name) else {
        bail!("that name is not a usable operator — use letters, digits and hyphen");
    };
    let mut users = sdroxide_config::load_users();
    let Some(i) = users.find(&safe) else {
        bail!("{safe} is not on the roster; use --add-user to put them on it");
    };
    let password = read_password(&safe)?;
    let password_hash = sdroxide_server::hash_password(&password).map_err(|e| anyhow::anyhow!(e))?;
    users.users[i].password_hash = password_hash;
    users.users[i].password.clear();
    write_roster(&users)?;
    eprintln!("updated {safe}'s password");
    Ok(())
}
