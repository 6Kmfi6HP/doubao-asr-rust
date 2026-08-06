use clap::Parser;
use doubao_asr::Client;
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

#[derive(Parser)]
#[command(
    name = "doubao-asr",
    version = env!("CARGO_PKG_VERSION"),
    about = "Transcribe the first audio stream in a local file with Doubao IME ASR."
)]
struct Args {
    /// Credential file path (default: OS user config directory)
    #[arg(long)]
    credentials: Option<PathBuf>,
    /// Also write the transcript to this file
    #[arg(long)]
    output: Option<PathBuf>,
    /// Emit structured JSON
    #[arg(long)]
    json: bool,
    /// Enable punctuation
    #[arg(long, default_value_t=true, action=clap::ArgAction::Set)]
    punctuation: bool,
    /// Upload pacing multiplier (0 < speed <= 4)
    #[arg(long, default_value_t = 1.0)]
    speed: f64,
    /// Audio or video input file
    input: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> doubao_asr::Result<()> {
    let args = Args::parse();
    let mut builder = Client::builder()
        .punctuation(args.punctuation)
        .upload_speed(args.speed);
    if let Some(path) = args.credentials {
        builder = builder.credentials_path(path);
    }
    let client = builder.build()?;
    eprintln!("Transcribing with Doubao ASR...");
    let transcript = client.transcribe_file(args.input).await?;
    let output = if args.json {
        let mut data = serde_json::to_vec(&transcript)
            .map_err(|e| doubao_asr::Error::Message(format!("encode transcript: {e}")))?;
        data.push(b'\n');
        data
    } else {
        format!("{}\n", transcript.text).into_bytes()
    };
    if let Some(path) = args.output {
        write_atomic(&path, &output).await?;
    }
    use tokio::io::AsyncWriteExt;
    tokio::io::stdout().write_all(&output).await?;
    Ok(())
}

async fn write_atomic(path: &Path, data: &[u8]) -> doubao_asr::Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = absolute
        .parent()
        .ok_or_else(|| doubao_asr::Error::Message("output path has no parent".into()))?;
    tokio::fs::create_dir_all(parent).await?;
    let temp = parent.join(format!(".transcript.{}.tmp", uuid::Uuid::new_v4()));
    let result = async {
        use tokio::io::AsyncWriteExt;
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let mut file = options.open(&temp).await?;
        file.write_all(data).await?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temp, &absolute).await?;
        Ok::<_, std::io::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn output_is_atomic_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/result.txt");
        write_atomic(&path, "测试文本\n".as_bytes()).await.unwrap();
        assert_eq!(
            tokio::fs::read(&path).await.unwrap(),
            "测试文本\n".as_bytes()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
