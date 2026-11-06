use jgt_voice::{check_piper, Speaker, VoiceEvent};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let text = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Hello from the Coordinator".to_string());

    let cfg = jgt_core::config::Voice::default();
    println!("Voice configuration: tts_command={}, tts_voice={}", cfg.tts_command, cfg.tts_voice);

    let voice_path = match check_piper(&cfg.tts_voice) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("\n{err}\n");
            eprintln!("To test real speech playback, download the Piper voice model first.");
            return Ok(());
        }
    };

    println!("Using Piper voice at: {}", voice_path.display());
    println!("Starting speaker and playback stream...");

    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let speaker = match Speaker::start(&cfg, tx) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("\nFailed to start speaker: {err}\n");
            return Ok(());
        }
    };

    speaker.set_on_speaking(|is_speaking| {
        println!("[Speaking callback] is_speaking={is_speaking}");
    });

    println!("Speaking: \"{text}\"");
    speaker.speak(&text)?;

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(VoiceEvent::Speaking) => println!("[Voice] Speaking started..."),
                    Some(VoiceEvent::SpeakingDone) => {
                        println!("[Voice] Speaking completed.");
                        break;
                    }
                    Some(VoiceEvent::Error) => {
                        eprintln!("[Voice] Error occurred during speech synthesis/playback");
                        break;
                    }
                    Some(other) => println!("[Voice] Event: {other:?}"),
                    None => break,
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("\nInterrupted! Stopping speaker...");
                speaker.stop();
                break;
            }
        }
    }

    Ok(())
}
