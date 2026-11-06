use jgt_voice::{check_whisper, Listener, VoiceEvent};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cfg = jgt_core::config::Voice::default();
    println!("Voice configuration: stt_model={}", cfg.stt_model);

    let model_path = match check_whisper(&cfg.stt_model) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("\n{err}\n");
            eprintln!("To test real microphone transcription, download the model first.");
            return Ok(());
        }
    };

    println!("Using Whisper model at: {}", model_path.display());
    println!("Starting audio capture and VAD...");

    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let handle = Listener::start(&cfg, tx)?;

    println!("Listening from microphone... Speak into your mic! (Press Ctrl+C to exit)");

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(VoiceEvent::Listening) => println!("[Voice] Listening..."),
                    Some(VoiceEvent::Hearing) => println!("[Voice] Hearing you..."),
                    Some(VoiceEvent::Transcribing) => println!("[Voice] Transcribing..."),
                    Some(VoiceEvent::NoAudio) => eprintln!("[Voice] No audio from microphone"),
                    Some(VoiceEvent::Muted) => println!("[Voice] Muted"),
                    Some(VoiceEvent::UserSpeech(text)) => println!("\n>>> Transcribed: \"{}\"\n", text),
                    Some(VoiceEvent::Speaking) => println!("[Voice] Speaking"),
                    Some(VoiceEvent::SpeakingDone) => println!("[Voice] Speaking done"),
                    Some(VoiceEvent::Error) => eprintln!("[Voice] Error occurred in pipeline"),
                    None => break,
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("\nShutting down listener...");
                handle.stop();
                break;
            }
        }
    }

    Ok(())
}
