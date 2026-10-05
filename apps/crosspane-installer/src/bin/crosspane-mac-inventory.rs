//! Build-time inventory only. Nothing here selects a live install or runs payload executables.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("crosspane-mac-inventory: macOS build tool only");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = mac::run() {
        eprintln!("crosspane-mac-inventory: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use anyhow::{Context, Result, bail, ensure};
    use clap::Parser;
    use crosspane_installer::platform::macos::{
        native_io::MAX_FILE_BYTES,
        payload::{
            ApprovedInventory, MAX_PAYLOAD_BYTES, MAX_PAYLOAD_FILES, PayloadFile, PayloadRole,
            SigningRule,
        },
    };
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs::{self, Metadata, OpenOptions},
        io::{Read, Seek, SeekFrom, Write},
        os::unix::fs::{MetadataExt, OpenOptionsExt},
        path::{Path, PathBuf},
        process::{Command, Stdio},
    };

    const APPLE_DEVELOPMENT: &str =
        "=anchor apple generic and certificate leaf[subject.CN] = \"Apple Development:\"*";
    const OUTPUT_LIMIT: usize = 64 * 1024;

    #[derive(Parser)]
    struct Args {
        payload: PathBuf,
        #[arg(long)]
        product_version: String,
        #[arg(long, value_delimiter = ',')]
        features: Vec<String>,
        /// Optional build-selected Team; every executable must belong to that same Team.
        #[arg(long)]
        team_id: Option<String>,
    }

    fn output(args: &[&str], path: &Path) -> Result<std::process::Output> {
        let result = Command::new("/usr/bin/codesign")
            .args(args)
            .arg(path)
            .output()?;
        ensure!(
            result.stdout.len() + result.stderr.len() <= OUTPUT_LIMIT,
            "signature output too large"
        );
        ensure!(
            result.status.success(),
            "signature check/query refused ({args:?}): {}: {}",
            path.display(),
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(result)
    }

    fn field(text: &str, key: &str) -> Result<String> {
        let values: Vec<_> = text.lines().filter_map(|l| l.strip_prefix(key)).collect();
        ensure!(
            values.len() == 1 && !values[0].is_empty(),
            "missing or repeated signature field: {key}"
        );
        Ok(values[0].to_owned())
    }

    fn identity(m: &Metadata) -> [u64; 10] {
        [
            m.dev(),
            m.ino(),
            u64::from(m.uid()),
            u64::from(m.mode()),
            m.nlink(),
            m.len(),
            m.mtime() as u64,
            m.mtime_nsec() as u64,
            m.ctime() as u64,
            m.ctime_nsec() as u64,
        ]
    }

    fn hash_file(path: &Path, before: &Metadata) -> Result<[u8; 32]> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path)?;
        ensure!(
            identity(&file.metadata()?) == identity(before),
            "file changed before hashing"
        );
        let mut hash = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
        let mut total = 0usize;
        let mut block = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut block)?;
            if n == 0 {
                break;
            }
            total = total.checked_add(n).context("file size overflow")?;
            ensure!(total <= MAX_FILE_BYTES, "payload file too large");
            hash.update(&block[..n]);
        }
        ensure!(
            total as u64 == before.len()
                && identity(&file.metadata()?) == identity(before)
                && identity(&fs::symlink_metadata(path)?) == identity(before),
            "file changed while hashing"
        );
        Ok(hash.finish().as_ref().try_into()?)
    }

    fn entitlements(path: &Path) -> Result<BTreeMap<String, bool>> {
        let result = output(&["-d", "--entitlements", ":-"], path)?;
        if result.stdout.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut child = Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-", "--", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .context("plist input unavailable")?
            .write_all(&result.stdout)?;
        let json = child.wait_with_output()?;
        ensure!(
            json.status.success() && json.stdout.len() <= OUTPUT_LIMIT,
            "invalid entitlements plist"
        );
        serde_json::from_slice(&json.stdout).context("entitlements must be a boolean dictionary")
    }

    fn rule(path: &Path, role: PayloadRole, team: &mut Option<String>) -> Result<SigningRule> {
        output(
            &[
                "--verify",
                "--strict",
                "--all-architectures",
                "-R",
                APPLE_DEVELOPMENT,
            ],
            path,
        )?;
        let details = output(&["-dvvv"], path)?;
        let text = std::str::from_utf8(&details.stderr)?;
        ensure!(
            !text.lines().any(|l| l == "Signature=adhoc") && text.contains("(runtime)"),
            "code must have hardened Apple Development signing"
        );
        let found_team = field(text, "TeamIdentifier=")?;
        ensure!(
            found_team.len() == 10
                && found_team
                    .bytes()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
            "invalid Team ID"
        );
        if let Some(expected) = team {
            ensure!(
                *expected == found_team,
                "payload code belongs to a different Team"
            );
        } else {
            *team = Some(found_team);
        }
        let requirements = output(&["-d", "-r-"], path)?;
        let requirement = format!(
            "{}\n{}",
            std::str::from_utf8(&requirements.stdout)?,
            std::str::from_utf8(&requirements.stderr)?
        );
        let designated_requirement = field(&requirement, "designated => ")?;
        let identifier = field(text, "Identifier=")?;
        let entitlements = entitlements(path)?;
        ensure!(
            role == PayloadRole::Agent || entitlements.is_empty(),
            "helper code has unexpected entitlements"
        );
        mach_o(path, role == PayloadRole::EmbeddedCode)?;
        Ok(SigningRule {
            role,
            identifier,
            designated_requirement,
            entitlements,
        })
    }

    /// The producer's architecture rule: one arm64 slice (thin, or in a fat32/fat64 container)
    /// whose file type is an executable, or a dylib for embedded code.
    fn mach_o(path: &Path, embedded: bool) -> Result<()> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path)?;
        let mut head = Vec::with_capacity(8 + 16 * 32);
        (&mut file).take(8 + 16 * 32).read_to_end(&mut head)?;
        let be = |at: usize| -> Result<u64> {
            let word: [u8; 4] = head
                .get(at..at + 4)
                .context("truncated Mach-O header")?
                .try_into()?;
            Ok(u64::from(u32::from_be_bytes(word)))
        };
        let offset = match be(0)? {
            magic @ (0xcafe_babe | 0xcafe_babf) => {
                let (count, stride) = (be(4)? as usize, if magic == 0xcafe_babe { 20 } else { 32 });
                ensure!((1..=16).contains(&count), "invalid fat Mach-O");
                let mut arm = None;
                for n in 0..count {
                    let at = 8 + n * stride;
                    if be(at)? == 0x0100_000c {
                        ensure!(arm.is_none(), "repeated arm64 slice");
                        arm = Some(if stride == 20 {
                            be(at + 8)?
                        } else {
                            (be(at + 8)? << 32) | be(at + 12)?
                        });
                    }
                }
                arm.context("payload code must contain arm64")?
            }
            _ => 0,
        };
        file.seek(SeekFrom::Start(offset))?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header)?;
        let le = |at: usize| {
            u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
        };
        ensure!(
            le(0) == 0xfeed_facf && le(4) == 0x0100_000c && le(12) == if embedded { 6 } else { 2 },
            "payload code must be an arm64 {}",
            if embedded { "dylib" } else { "executable" }
        );
        Ok(())
    }

    fn role(path: &str) -> Result<Option<PayloadRole>> {
        Ok(match path {
            "Crosspane.app/Contents/MacOS/Crosspane" => Some(PayloadRole::Agent),
            "Crosspane.app/Contents/MacOS/crosspane-ui" => Some(PayloadRole::Settings),
            "crosspanectl" => Some(PayloadRole::Ctl),
            "crosspane-installer" => Some(PayloadRole::Installer),
            other
                if other.starts_with("Crosspane.app/Contents/Frameworks/")
                    && other.ends_with(".dylib") =>
            {
                Some(PayloadRole::EmbeddedCode)
            }
            other
                if other.starts_with("Crosspane.app/Contents/MacOS/")
                    || other.starts_with("Crosspane.app/Contents/Frameworks/") =>
            {
                bail!("unrecognized executable: {other}")
            }
            _ => None,
        })
    }

    fn collect(root: &Path) -> Result<(BTreeMap<String, Metadata>, BTreeSet<String>)> {
        let uid = rustix::process::geteuid().as_raw();
        ensure!(
            uid != 0 && root.is_absolute() && fs::canonicalize(root)? == root,
            "use an absolute, non-symlink user-owned payload"
        );
        let mut pending = vec![root.to_owned()];
        let mut files = BTreeMap::new();
        let mut dirs = BTreeSet::new();
        while let Some(dir) = pending.pop() {
            let before = fs::symlink_metadata(&dir)?;
            ensure!(
                before.is_dir() && before.uid() == uid && before.mode() & 0o7022 == 0,
                "unsafe payload directory"
            );
            for child in fs::read_dir(&dir)? {
                let path = child?.path();
                let relative = path
                    .strip_prefix(root)?
                    .to_str()
                    .context("non-UTF8 payload name")?
                    .to_owned();
                ensure!(relative.split('/').count() <= 16, "payload tree too deep");
                let m = fs::symlink_metadata(&path)?;
                ensure!(
                    m.uid() == uid && !m.file_type().is_symlink(),
                    "foreign or symlink payload member"
                );
                if m.is_dir() {
                    ensure!(
                        dirs.len() < MAX_PAYLOAD_FILES * 4,
                        "too many payload directories"
                    );
                    dirs.insert(relative);
                    pending.push(path);
                } else {
                    ensure!(
                        m.is_file()
                            && m.nlink() == 1
                            && m.len() > 0
                            && m.len() <= MAX_FILE_BYTES as u64
                            && files.len() < MAX_PAYLOAD_FILES,
                        "invalid or excessive payload file"
                    );
                    files.insert(relative, m);
                }
            }
            ensure!(
                identity(&fs::symlink_metadata(dir)?) == identity(&before),
                "payload directory changed during traversal"
            );
        }
        Ok((files, dirs))
    }

    pub(super) fn run() -> Result<()> {
        let args = Args::parse();
        let (files, dirs) = collect(&args.payload)?;
        let mut team = args.team_id;
        let mut inventory = ApprovedInventory {
            product_version: args.product_version,
            features: args.features,
            files: Vec::new(),
        };
        let mut total = 0u64;
        let mut expected_dirs = BTreeSet::new();
        output(
            &["--verify", "--deep", "--strict"],
            &args.payload.join("Crosspane.app"),
        )?;
        for (relative, metadata) in files {
            total = total
                .checked_add(metadata.len())
                .context("payload size overflow")?;
            ensure!(total <= MAX_PAYLOAD_BYTES, "payload too large");
            let path = args.payload.join(&relative);
            let signing = role(&relative)?
                .map(|r| rule(&path, r, &mut team))
                .transpose()?;
            let mode = metadata.mode() & 0o7777;
            ensure!(
                mode == if signing.is_some() { 0o755 } else { 0o644 },
                "incorrect mode for {relative}"
            );
            let sha256 = hash_file(&path, &metadata)?;
            let mut parent = Path::new(&relative).parent();
            while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
                expected_dirs.insert(p.to_str().context("non-UTF8 directory")?.to_owned());
                parent = p.parent();
            }
            inventory.files.push(PayloadFile {
                path: relative,
                size: metadata.len(),
                sha256,
                mode,
                signing,
            });
        }
        ensure!(
            dirs == expected_dirs,
            "payload has undeclared empty directories"
        );
        inventory
            .validate()
            .map_err(|e| anyhow::anyhow!("approved inventory validation failed: {e:?}"))?;
        println!("{}", serde_json::to_string(&inventory)?);
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn signature_fields_require_exactly_one_value() {
            assert_eq!(
                field("Identifier=a\nTeamIdentifier=1234567890", "Identifier=").unwrap(),
                "a"
            );
            for text in ["", "Identifier=", "Identifier=a\nIdentifier=b"] {
                assert!(field(text, "Identifier=").is_err());
            }
        }
        #[test]
        fn executable_roles_are_fixed_and_unknown_code_refuses() {
            assert_eq!(
                role("crosspane-installer").unwrap(),
                Some(PayloadRole::Installer)
            );
            assert_eq!(
                role("Crosspane.app/Contents/Frameworks/libopus.0.dylib").unwrap(),
                Some(PayloadRole::EmbeddedCode)
            );
            assert!(role("Crosspane.app/Contents/MacOS/other").is_err());
            assert!(role("Crosspane.app/Contents/Frameworks/other").is_err());
            assert_eq!(
                role("Crosspane.app/Contents/Resources/audio/packages.json").unwrap(),
                None
            );
        }
    }
}
