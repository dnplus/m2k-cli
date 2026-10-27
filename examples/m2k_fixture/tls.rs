use anyhow::{bail, Context, Result};
use std::{fs, path::Path, process::Command};

/// Generates a throwaway CA and a loopback server certificate with the
/// `openssl` CLI, so the fixture adds no crypto dependency to Cargo.lock.
/// Short validity keeps macOS' TLS server certificate rules satisfied.
pub fn generate(dir: &Path) -> Result<native_tls::TlsAcceptor> {
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let ext = dir.join("server.ext");
    fs::write(
        &ext,
        "basicConstraints=critical,CA:FALSE\n\
         keyUsage=critical,digitalSignature,keyEncipherment\n\
         extendedKeyUsage=serverAuth\n\
         subjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1\n",
    )?;
    let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
    openssl(&[
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-days",
        "30",
        "-subj",
        "/CN=m2k fixture test CA",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
        "-addext",
        "keyUsage=critical,keyCertSign,cRLSign",
        "-keyout",
        &path("ca.key"),
        "-out",
        &path("ca.pem"),
    ])?;
    openssl(&[
        "req",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-subj",
        "/CN=localhost",
        "-keyout",
        &path("server.key"),
        "-out",
        &path("server.csr"),
    ])?;
    openssl(&[
        "x509",
        "-req",
        "-in",
        &path("server.csr"),
        "-CA",
        &path("ca.pem"),
        "-CAkey",
        &path("ca.key"),
        "-set_serial",
        &serial(),
        "-days",
        "30",
        "-extfile",
        &path("server.ext"),
        "-out",
        &path("server.pem"),
    ])?;
    let cert = fs::read(dir.join("server.pem"))?;
    let key = fs::read(dir.join("server.key"))?;
    let identity =
        native_tls::Identity::from_pkcs8(&cert, &key).context("cannot load fixture identity")?;
    native_tls::TlsAcceptor::new(identity).context("cannot build fixture TLS acceptor")
}

fn serial() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(1)
        .to_string()
}

fn openssl(args: &[&str]) -> Result<()> {
    let output = Command::new("openssl")
        .args(args)
        .output()
        .context("fixture TLS needs the `openssl` CLI on PATH")?;
    if !output.status.success() {
        bail!(
            "openssl {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
