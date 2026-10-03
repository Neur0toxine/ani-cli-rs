use std::{
    fmt,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::Stdio,
};

use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::{
    AniError, Result, StreamLink, SubtitleTrack, relay_stream, relay_stream_without_hls_subtitles,
    requires_hls_relay,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayerKind {
    Mpv,
    Iina,
    Vlc,
    AndroidMpv,
    AndroidVlc,
    Syncplay,
    Custom,
}

impl fmt::Display for PlayerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Mpv => "mpv",
            Self::Iina => "iina",
            Self::Vlc => "vlc",
            Self::AndroidMpv => "android-mpv",
            Self::AndroidVlc => "android-vlc",
            Self::Syncplay => "syncplay",
            Self::Custom => "custom",
        };
        f.write_str(label)
    }
}

#[derive(Clone, Debug)]
pub struct PlayerOptions {
    pub executable: PathBuf,
    pub kind: PlayerKind,
    pub no_detach: bool,
    pub exit_after_play: bool,
    pub force_hls_relay: bool,
}

impl PlayerOptions {
    pub fn default_player() -> Self {
        if cfg!(target_os = "android") {
            Self::default_android_mpv()
        } else if cfg!(target_os = "macos") {
            Self::default_iina()
        } else {
            Self::default_mpv()
        }
    }

    pub fn default_mpv() -> Self {
        let executable = std::env::var_os("ANI_CLI_PLAYER")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "mpv.exe" } else { "mpv" }));
        Self {
            executable,
            kind: PlayerKind::Mpv,
            no_detach: env_bool("ANI_CLI_NO_DETACH"),
            exit_after_play: env_bool("ANI_CLI_EXIT_AFTER_PLAY"),
            force_hls_relay: false,
        }
    }

    pub fn default_iina() -> Self {
        let executable = std::env::var_os("ANI_CLI_PLAYER")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("iina"));
        Self {
            executable,
            kind: PlayerKind::Iina,
            no_detach: env_bool("ANI_CLI_NO_DETACH"),
            exit_after_play: env_bool("ANI_CLI_EXIT_AFTER_PLAY"),
            force_hls_relay: false,
        }
    }

    pub fn default_android_mpv() -> Self {
        Self {
            executable: android_intent_launcher(),
            kind: PlayerKind::AndroidMpv,
            no_detach: true,
            exit_after_play: env_bool("ANI_CLI_EXIT_AFTER_PLAY"),
            force_hls_relay: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Player {
    options: PlayerOptions,
}

impl Player {
    pub fn new(options: PlayerOptions) -> Self {
        Self { options }
    }

    pub fn command_args(&self, stream: &StreamLink, title: &str) -> Vec<String> {
        self.command_args_inner(stream, title, self.options.no_detach)
    }

    /// Human-readable summary of the configured player (executable + kind).
    /// Useful for debug logs and error messages.
    pub fn describe(&self) -> String {
        format!(
            "{} ({}) [no_detach={}, exit_after_play={}, force_hls_relay={}]",
            self.options.executable.display(),
            self.options.kind,
            self.options.no_detach,
            self.options.exit_after_play,
            self.options.force_hls_relay,
        )
    }

    fn command_args_inner(&self, stream: &StreamLink, title: &str, attached: bool) -> Vec<String> {
        let referer = stream.headers.referer.as_deref().unwrap_or("");
        match self.options.kind {
            PlayerKind::Mpv => {
                let mut args = mpv_options(stream, title, referer);
                // Ensure URL is passed last
                args.push(stream.url.clone());
                args
            }
            PlayerKind::Iina => {
                let mut args = vec!["--no-stdin".into()];
                if attached {
                    args.push("--keep-running".into());
                }
                args.push(stream.url.clone());
                args.push("--".into());
                args.extend(mpv_options(stream, title, referer));
                args
            }
            PlayerKind::Vlc => {
                let mut args = vec!["--play-and-exit".into(), format!("--meta-title={title}")];
                if !referer.is_empty() {
                    args.push(format!("--http-referrer={referer}"));
                }
                if let Some(agent) = stream.headers.extra.get("User-Agent") {
                    args.push(format!("--http-user-agent={agent}"));
                }
                for track in &stream.subtitles {
                    args.push(format!("--sub-file={}", track.url));
                }
                args.push(stream.url.clone());
                args
            }
            PlayerKind::AndroidMpv => {
                android_intent_args("is.xyz.mpv/.MPVActivity", &stream.url, title)
            }
            PlayerKind::AndroidVlc => android_intent_args(
                "org.videolan.vlc/org.videolan.vlc.gui.video.VideoPlayerActivity",
                &stream.url,
                title,
            ),
            PlayerKind::Syncplay => {
                let mut args = vec![
                    stream.url.clone(),
                    "--".into(),
                    "--tls-verify=no".into(),
                    format!("--force-media-title={title}"),
                ];
                if !referer.is_empty() {
                    args.push(format!("--referrer={referer}"));
                }
                append_mpv_headers(&mut args, stream);
                let mut subtitles: Vec<&SubtitleTrack> = stream.subtitles.iter().collect();
                subtitles.sort_by_key(|track| track.default);
                for track in subtitles {
                    args.push(format!("--sub-file={}", track.url));
                }
                args
            }
            PlayerKind::Custom => vec![stream.url.clone()],
        }
    }

    pub async fn play(&self, stream: &StreamLink, title: &str) -> Result<Option<i32>> {
        info!(
            title = %title,
            player = %self.describe(),
            stream_url = %stream.url,
            hls = stream.hls,
            subtitles = stream.subtitles.len(),
            "playback requested",
        );
        // Convert upstream HTTPS subtitles before rewriting URLs to loopback
        // HTTP. Keep the ASS server alive until the player exits.
        let prepared = if !stream.subtitles.is_empty()
            && matches!(
                self.options.kind,
                PlayerKind::Mpv | PlayerKind::Iina | PlayerKind::Syncplay | PlayerKind::Vlc
            ) {
            Some(crate::hls_relay::prepare_desktop_subtitles(stream).await?)
        } else {
            None
        };
        let stream = prepared.as_ref().map_or(stream, |(_, local)| local);
        if self.options.force_hls_relay
            || crate::requires_hls_relay(stream)
            || (self.is_android_player() && stream.hls)
        {
            debug!(
                title = %title,
                player = %self.options.kind.to_string(),
                android_player = self.is_android_player(),
                hls = stream.hls,
                "stream requires the loopback HLS relay",
            );
            // Android players receive a single intent URL and cannot be given
            // an explicit `--sub-file`, so they need subtitles exposed as
            // synthetic HLS renditions. Desktop players already receive
            // subtitles via `--sub-file`, and wrapping a long subtitle file as
            // a single oversized HLS segment produces unreliable cue timing
            // in some HLS demuxers (see issue #18).
            let mut relay_source = stream.clone();
            if prepared.is_some() {
                // Prepared tracks already have their own in-memory server.
                relay_source.subtitles.clear();
            }
            let (_relay, mut local) = if self.is_android_player() {
                relay_stream(&relay_source).await?
            } else {
                relay_stream_without_hls_subtitles(&relay_source).await?
            };
            if prepared.is_some() {
                local.subtitles = stream.subtitles.clone();
            }
            debug!(
                title = %title,
                local_url = %local.url,
                "HLS relay is serving the rewritten stream URL to the player",
            );
            return self.play_inner(&local, title, true).await;
        }
        self.play_inner(stream, title, prepared.is_some()).await
    }

    async fn play_inner(
        &self,
        stream: &StreamLink,
        title: &str,
        force_attached: bool,
    ) -> Result<Option<i32>> {
        if self.is_android_player() {
            return self.play_android(stream, title, force_attached).await;
        }

        // Validate that the player executable exists before attempting to launch
        // For simple names (e.g., "mpv"), check if they exist in PATH
        let needs_validation = self.options.executable.components().count() > 1;
        if needs_validation {
            if !self.options.executable.exists() {
                eprintln!(
                    "Player executable not found: {}. Please install the player or set ANI_CLI_PLAYER environment variable.",
                    self.options.executable.display()
                );
                return Err(AniError::PlayerNotFound);
            }
        } else {
            // For simple names, check if they can be found in PATH
            use which::which;
            if which(&self.options.executable).is_err() {
                eprintln!(
                    "Player executable '{}' not found in PATH. Please install the player or set ANI_CLI_PLAYER environment variable.",
                    self.options.executable.display()
                );
                return Err(AniError::PlayerNotFound);
            }
        }

        let mut command = Command::new(&self.options.executable);
        let attached = self.options.no_detach || force_attached;
        // sub-add supplies title/language metadata that bare ASS sidecar URLs
        // lack. Load before mpv selects tracks so slang and user scripts work.
        let subtitle_script = if !stream.subtitles.is_empty()
            && matches!(
                self.options.kind,
                PlayerKind::Mpv | PlayerKind::Iina | PlayerKind::Syncplay
            ) {
            Some(mpv_subtitle_script(&stream.subtitles)?)
        } else {
            None
        };
        let mut args = self.command_args_inner(stream, title, attached);
        if let Some(script) = &subtitle_script {
            args.retain(|arg| !arg.starts_with("--sub-file="));
            let option = format!("--scripts-append={}", script.path().display());
            // mpv's media URL is last; IINA/Syncplay's raw options follow `--`.
            if self.options.kind == PlayerKind::Mpv {
                args.insert(args.len() - 1, option);
            } else {
                args.push(option);
            }
        }
        info!(
            title = %title,
            executable = %self.options.executable.display(),
            kind = %self.options.kind,
            attached,
            "launching external player",
        );
        // The URL is logged at debug level so that long playlist URLs do not
        // clutter the default log output, but the rest of the player command
        // line (including the referer / user-agent switches) is logged at
        // debug level for the same reason.
        debug!(
            title = %title,
            stream_url = %stream.url,
            stream_hls = stream.hls,
            args = ?args,
            "full player command line",
        );
        command.args(args);
        if attached {
            match command.status().await {
                Ok(status) => {
                    let code = status.code().unwrap_or(1);
                    info!(
                        title = %title,
                        exit_code = code,
                        success = status.success(),
                        "player exited",
                    );
                    if !status.success() && self.options.exit_after_play {
                        eprintln!("Player exited with {code}");
                        return Err(AniError::PlayerExitFailed);
                    }
                    Ok(Some(code))
                }
                Err(error) => {
                    warn!(
                        title = %title,
                        executable = %self.options.executable.display(),
                        error = %error,
                        "failed to launch player in attached mode",
                    );
                    eprintln!(
                        "Could not launch {}: {error}",
                        self.options.executable.display()
                    );
                    Err(AniError::PlayerLaunchFailed)
                }
            }
        } else {
            // ensure proper process detachment
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            match command.spawn() {
                Ok(child) => {
                    info!(
                        title = %title,
                        pid = child.id().unwrap_or(0),
                        executable = %self.options.executable.display(),
                        "player launched in the background",
                    );
                    // Immediately detach the child process
                    let _ = child.id(); // Ensure the process handle is consumed
                    Ok(None)
                }
                Err(error) => {
                    warn!(
                        title = %title,
                        executable = %self.options.executable.display(),
                        error = %error,
                        "failed to launch player in detached mode",
                    );
                    eprintln!(
                        "Could not launch {}: {error}",
                        self.options.executable.display()
                    );
                    Err(AniError::PlayerLaunchFailed)
                }
            }
        }
    }

    fn is_android_player(&self) -> bool {
        matches!(
            self.options.kind,
            PlayerKind::AndroidMpv | PlayerKind::AndroidVlc
        )
    }

    async fn play_android(
        &self,
        stream: &StreamLink,
        title: &str,
        relay_active: bool,
    ) -> Result<Option<i32>> {
        let terminal = io::stdin().is_terminal();
        if relay_active && !terminal {
            return Err(AniError::PlayerAndroidTerminalRequired);
        }

        let launch_args = self.command_args_inner(stream, title, true);
        info!(
            title = %title,
            executable = %self.options.executable.display(),
            "launching Android activity for playback",
        );
        debug!(
            title = %title,
            stream_url = %stream.url,
            args = ?launch_args,
            "full Android intent command line",
        );
        let launch_result = Command::new(&self.options.executable)
            .args(launch_args)
            .status()
            .await;
        let code = match launch_result {
            Ok(status) if status.success() => {
                let code = status.code().unwrap_or(0);
                info!(
                    title = %title,
                    exit_code = code,
                    "Android player activity returned successfully",
                );
                code
            }
            result => {
                let primary_error = match &result {
                    Ok(status) => format!(
                        "Android activity launcher {} exited with {}",
                        self.options.executable.display(),
                        status.code().unwrap_or(1)
                    ),
                    Err(error) => format!(
                        "could not launch Android player through {}: {error}",
                        self.options.executable.display()
                    ),
                };
                warn!(
                    title = %title,
                    executable = %self.options.executable.display(),
                    error = %primary_error,
                    "primary Android launch failed; trying termux-open fallback",
                );
                match launch_android_url_fallback(&self.options.executable, &stream.url, stream.hls)
                    .await
                {
                    Ok(code) => {
                        warn!(
                            title = %title,
                            "opened the stream through Android's default URL handler instead",
                        );
                        eprintln!(
                            "warning: {primary_error}; opened the stream through Android's default URL handler instead"
                        );
                        code
                    }
                    Err(fallback_error) => {
                        warn!(
                            title = %title,
                            error = %fallback_error,
                            "termux-open fallback also failed",
                        );
                        eprintln!("{primary_error}; {fallback_error}");
                        return Err(AniError::PlayerLaunchFailed);
                    }
                }
            }
        };

        if terminal {
            debug!(
                title = %title,
                "waiting for the Android player to finish before returning control to the TUI",
            );
            wait_for_android_player().await?;
        }
        Ok(Some(code))
    }
}

fn android_intent_args(component: &str, url: &str, title: &str) -> Vec<String> {
    vec![
        "start".into(),
        "--user".into(),
        "0".into(),
        "-a".into(),
        "android.intent.action.VIEW".into(),
        "-d".into(),
        url.into(),
        "-n".into(),
        component.into(),
        "--es".into(),
        "title".into(),
        title.into(),
    ]
}

fn android_intent_launcher() -> PathBuf {
    if let Some(executable) = std::env::var_os("ANI_CLI_PLAYER") {
        return PathBuf::from(executable);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    ["termux-am-starter", "termux-am", "am"]
        .iter()
        .find_map(|name| find_in_path(name, &path))
        .unwrap_or_else(|| PathBuf::from("termux-am-starter"))
}

fn find_in_path(executable: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|directory| directory.join(executable))
        .find(|candidate| candidate.is_file())
}

async fn launch_android_url_fallback(
    executable: &Path,
    url: &str,
    hls: bool,
) -> std::result::Result<i32, String> {
    if !is_termux_activity_launcher(executable) {
        return Err(
            "the configured custom launcher failed and cannot use the automatic Termux fallback"
                .into(),
        );
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut open_error = None;
    if let Some(opener) = find_in_path("termux-open", &path) {
        let status = Command::new(&opener)
            .args(["--view", "--content-type", android_media_type(hls), url])
            .status()
            .await;
        match status {
            Ok(status) if status.success() => return Ok(status.code().unwrap_or(0)),
            Ok(status) => {
                open_error = Some(format!(
                    "{} exited with {}",
                    opener.display(),
                    status.code().unwrap_or(1)
                ));
            }
            Err(error) => {
                open_error = Some(format!("could not run {}: {error}", opener.display()));
            }
        }
    }
    let opener = find_in_path("termux-open-url", &path).ok_or_else(|| {
        let prefix = open_error
            .map(|error| format!("{error}; "))
            .unwrap_or_default();
        format!(
            "{prefix}termux-open-url is unavailable; install or update the termux-tools package"
        )
    })?;
    let status = Command::new(&opener)
        .arg(url)
        .status()
        .await
        .map_err(|error| format!("could not run {}: {error}", opener.display()))?;
    if !status.success() {
        return Err(format!(
            "{} exited with {}",
            opener.display(),
            status.code().unwrap_or(1)
        ));
    }
    Ok(status.code().unwrap_or(0))
}

fn android_media_type(hls: bool) -> &'static str {
    if hls {
        "application/vnd.apple.mpegurl"
    } else {
        "video/mp4"
    }
}

fn is_termux_activity_launcher(executable: &Path) -> bool {
    executable
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "termux-am-starter" | "termux-am" | "am"))
}

async fn wait_for_android_player() -> Result<()> {
    tokio::task::spawn_blocking(|| {
        println!(
            "Opened the Android player. Return to Termux and press Enter after playback ends."
        );
        print!("Waiting for Android player... ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        Ok::<(), io::Error>(())
    })
    .await
    .map_err(|error| {
        eprintln!("Android playback prompt failed: {error}");
        AniError::PlayerLaunchFailed
    })??;
    Ok(())
}

fn mpv_subtitle_script(tracks: &[SubtitleTrack]) -> Result<tempfile::NamedTempFile> {
    let mut script = tempfile::Builder::new()
        .prefix("ani-subtitles-")
        .suffix(".lua")
        .tempfile()?;
    script.write_all(mpv_subtitle_script_body(tracks)?.as_bytes())?;
    script.flush()?;
    Ok(script)
}

fn mpv_subtitle_script_body(tracks: &[SubtitleTrack]) -> Result<String> {
    let tracks: Vec<_> = tracks
        .iter()
        .map(|track| {
            serde_json::json!({
                "url": track.url,
                "title": track.label,
                "lang": crate::subtitles::language_code(&track.label),
                "default": track.default,
            })
        })
        .collect();
    let json = serde_json::to_string(&tracks)?;
    // A Lua long string preserves JSON escapes and Unicode. Choose a delimiter
    // absent from provider data so labels/URLs can never become script code.
    let mut delimiter = "=".to_owned();
    while json.contains(&format!("]{delimiter}]")) {
        delimiter.push('=');
    }
    Ok(format!(
        r#"local tracks = require('mp.utils').parse_json([{delimiter}[{json}]{delimiter}])
mp.add_hook('on_preloaded', 50, function()
    for _, track in ipairs(tracks) do
        local flags = track.default and 'auto+default' or 'auto'
        local _, err = mp.command_native({{'sub-add', track.url, flags, track.title, track.lang}})
        if err then mp.msg.warn('Could not load subtitle ' .. track.title .. ': ' .. tostring(err)) end
    end
end)
"#
    ))
}

fn mpv_options(stream: &StreamLink, title: &str, referer: &str) -> Vec<String> {
    let mut args = vec![
        "--tls-verify=no".into(),
        format!("--force-media-title={title}"),
    ];
    if !referer.is_empty() {
        args.push(format!("--referrer={referer}"));
    }
    append_mpv_headers(&mut args, stream);
    // Raw arguments remain usable without preparing subtitles. Playback
    // replaces these sidecars with a hook that supplies title/language metadata.
    let mut subtitles: Vec<&SubtitleTrack> = stream.subtitles.iter().collect();
    subtitles.sort_by_key(|track| track.default);
    for track in subtitles {
        args.push(format!("--sub-file={}", track.url));
    }
    // Add cache settings for HLS relay streams
    if stream.hls && requires_hls_relay(stream) {
        args.push("--cache=yes".into());
        args.push("--cache-secs=120".into());
        args.push("--demuxer-max-bytes=512MiB".into());
        args.push("--demuxer-max-back-bytes=256MiB".into());
    }
    args
}

fn append_mpv_headers(args: &mut Vec<String>, stream: &StreamLink) {
    let mut headers = Vec::new();
    if let Some(origin) = &stream.headers.origin
        && safe_header_value(origin)
    {
        headers.push(format!("Origin: {origin}"));
    }
    headers.extend(
        stream
            .headers
            .extra
            .iter()
            .filter(|(name, value)| safe_header_value(name) && safe_header_value(value))
            .map(|(name, value)| format!("{name}: {value}")),
    );
    if !headers.is_empty() {
        args.push(format!("--http-header-fields={}", headers.join(",")));
    }
}

fn safe_header_value(value: &str) -> bool {
    !value.contains(['\r', '\n'])
}

fn env_bool(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RequestHeaders, SubtitleTrack};
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires mpv with Lua support and FFmpeg"]
    async fn mpv_repaired_subtitles_have_metadata_and_can_be_selected() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        for locale in ["english", "russian"] {
            Mock::given(method("GET"))
                .and(path(format!("/{locale}.vtt")))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(
                        "WEBVTT\n\n00:00.000 --> 00:02.000\n<b>First\n\nSecond</b>\n",
                    ),
                )
                .mount(&server)
                .await;
        }
        let mut source = StreamLink {
            url: "https://example.invalid/video.mp4".into(),
            resolution: "auto".into(),
            hls: false,
            provider: "test".into(),
            downloadable: false,
            headers: RequestHeaders::default(),
            subtitles: vec![
                SubtitleTrack {
                    label: "English".into(),
                    url: format!("{}/english.vtt", server.uri()),
                    default: true,
                },
                SubtitleTrack {
                    label: "Русский".into(),
                    url: format!("{}/russian.vtt", server.uri()),
                    default: false,
                },
            ],
        };
        let directory = tempfile::tempdir().unwrap();
        // Two seconds of silent PCM provide a deterministic, offline media file.
        let audio = directory.path().join("audio.wav");
        let size = 8000u32 * 2 * 2;
        let mut wav = b"RIFF".to_vec();
        wav.extend((size + 36).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(8000u32.to_le_bytes());
        wav.extend(16000u32.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(size.to_le_bytes());
        wav.resize(size as usize + 44, 0);
        std::fs::write(&audio, wav).unwrap();
        let segment = directory.path().join("audio.ts");
        assert!(
            Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&audio)
                .args(["-c:a", "aac", "-f", "mpegts"])
                .arg(&segment)
                .status()
                .await
                .unwrap()
                .success()
        );
        Mock::given(method("GET"))
            .and(path("/audio.ts"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(std::fs::read(&segment).unwrap()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/video.m3u8"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "#EXTM3U\n#EXT-X-TARGETDURATION:3\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2.0,\naudio.ts\n#EXT-X-ENDLIST\n"
            )).mount(&server).await;
        source.url = format!("{}/video.m3u8", server.uri());
        source.hls = true;
        let socket = directory.path().join("mpv.sock");
        let log = directory.path().join("mpv.log");
        let executable = directory.path().join("mpv");
        let quote =
            |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
        std::fs::write(&executable, format!(
            "#!/bin/sh\nexec mpv --no-config --vo=null --ao=null --pause --idle=yes --slang=rus --input-ipc-server={} --log-file={} \"$@\" > /dev/null 2>&1\n",
            quote(&socket), quote(&log),
        )).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let player = Player::new(PlayerOptions {
            executable,
            kind: PlayerKind::Mpv,
            no_detach: true,
            exit_after_play: false,
            force_hls_relay: true,
        });
        // Exercise the actual playback sequence, including HLS rewriting,
        // rather than preparing tracks and launching mpv separately.
        let playback = tokio::spawn(async move { player.play(&source, "Fixture").await });
        let stream = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Ok(stream) = tokio::net::UnixStream::connect(&socket).await {
                    break stream;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let tracks = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                writer
                    .write_all(
                        b"{\"command\":[\"get_property\",\"track-list\"],\"request_id\":1}\n",
                    )
                    .await
                    .unwrap();
                while let Some(line) = lines.next_line().await.unwrap() {
                    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
                    if response["request_id"] == 1 {
                        let tracks: Vec<_> = response["data"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter(|track| track["type"] == "sub")
                            .cloned()
                            .collect();
                        if tracks.len() == 2 {
                            return tracks;
                        }
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(tracks[0]["title"], "English");
        assert_eq!(tracks[0]["lang"], "eng");
        assert_eq!(tracks[1]["title"], "Русский");
        assert_eq!(tracks[1]["lang"], "rus");
        assert_eq!(
            tracks[1]["selected"], true,
            "slang must override the provider default"
        );
        assert!(tracks.iter().all(|track| track["codec"] == "ass"));
        writer.write_all(b"{\"command\":[\"set_property\",\"sid\",1],\"request_id\":2}\n{\"command\":[\"get_property\",\"sid\"],\"request_id\":3}\n").await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(line) = lines.next_line().await.unwrap() {
                let response: serde_json::Value = serde_json::from_str(&line).unwrap();
                if response["request_id"] == 3 {
                    assert_eq!(response["data"], 1);
                    return;
                }
            }
            panic!("mpv IPC closed before confirming subtitle selection");
        })
        .await
        .unwrap();
        writer
            .write_all(b"{\"command\":[\"quit\"]}\n")
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), playback)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            Some(0)
        );
        assert!(
            !std::fs::read_to_string(&log)
                .unwrap()
                .contains("Could not load subtitle")
        );
    }

    #[test]
    fn subtitle_script_keeps_provider_data_out_of_lua_code() {
        let script = mpv_subtitle_script_body(&[SubtitleTrack {
            label: "Русский ]=] ' \\\"\n".into(),
            url: "https://example.invalid/sub?x=]=]".into(),
            default: true,
        }])
        .unwrap();
        assert!(script.contains("parse_json([==["));
        assert!(script.contains(r#"\n"#));
        assert!(script.contains(r#""lang":"und""#));
    }

    #[test]
    fn mpv_arguments_preserve_referrer_as_one_argument() {
        let player = Player::new(PlayerOptions {
            executable: "mpv".into(),
            kind: PlayerKind::Mpv,
            no_detach: true,
            exit_after_play: false,
            force_hls_relay: false,
        });
        let stream = StreamLink {
            url: "https://media/a.m3u8".into(),
            resolution: "1080p".into(),
            hls: true,
            provider: "Default".into(),
            downloadable: true,
            headers: RequestHeaders {
                referer: Some("https://ref.example".into()),
                ..Default::default()
            },
            subtitles: vec![],
        };
        assert!(
            player
                .command_args(&stream, "Anime Episode 1")
                .contains(&"--referrer=https://ref.example".into())
        );
    }

    #[test]
    fn mpv_appends_the_default_subtitle_last() {
        let player = Player::new(PlayerOptions {
            executable: "mpv".into(),
            kind: PlayerKind::Mpv,
            no_detach: true,
            exit_after_play: false,
            force_hls_relay: false,
        });
        let stream = StreamLink {
            url: "https://media/a.m3u8".into(),
            resolution: "1080p".into(),
            hls: true,
            provider: "Default".into(),
            downloadable: true,
            headers: RequestHeaders::default(),
            subtitles: vec![
                SubtitleTrack {
                    label: "Arabic".into(),
                    url: "http://127.0.0.1:1/r/one/Arabic".into(),
                    default: false,
                },
                SubtitleTrack {
                    label: "English".into(),
                    url: "http://127.0.0.1:1/r/two/English".into(),
                    default: true,
                },
                SubtitleTrack {
                    label: "Spanish".into(),
                    url: "http://127.0.0.1:1/r/three/Spanish".into(),
                    default: false,
                },
            ],
        };
        let args = player.command_args(&stream, "Anime");
        let sub_files: Vec<&String> = args
            .iter()
            .filter(|arg| arg.starts_with("--sub-file="))
            .collect();
        assert_eq!(sub_files.len(), 3);
        assert!(sub_files[0].ends_with("/Arabic"));
        assert!(sub_files[1].ends_with("/Spanish"));
        // Keep the existing ordering for callers using raw command arguments.
        assert!(sub_files[2].ends_with("/English"));
    }

    #[test]
    fn iina_arguments_put_stream_before_raw_mpv_options() {
        let player = Player::new(PlayerOptions {
            executable: "iina".into(),
            kind: PlayerKind::Iina,
            no_detach: false,
            exit_after_play: false,
            force_hls_relay: false,
        });
        let stream = StreamLink {
            url: "https://media/a.m3u8".into(),
            resolution: "1080p".into(),
            hls: true,
            provider: "Default".into(),
            downloadable: true,
            headers: RequestHeaders {
                referer: Some("https://ref.example".into()),
                origin: Some("https://origin.example".into()),
                ..Default::default()
            },
            subtitles: vec![SubtitleTrack {
                label: "English".into(),
                url: "https://media/subtitles.vtt".into(),
                default: true,
            }],
        };

        assert_eq!(
            player.command_args(&stream, "Anime Episode 1"),
            vec![
                "--no-stdin",
                "https://media/a.m3u8",
                "--",
                "--tls-verify=no",
                "--force-media-title=Anime Episode 1",
                "--referrer=https://ref.example",
                "--http-header-fields=Origin: https://origin.example",
                "--sub-file=https://media/subtitles.vtt",
                // The stream carries provider browser context, so it is
                // relayed and receives the HLS relay cache settings.
                "--cache=yes",
                "--cache-secs=120",
                "--demuxer-max-bytes=512MiB",
                "--demuxer-max-back-bytes=256MiB",
            ]
        );
    }

    #[test]
    fn forced_attached_iina_keeps_cli_running() {
        let player = Player::new(PlayerOptions {
            executable: "iina".into(),
            kind: PlayerKind::Iina,
            no_detach: false,
            exit_after_play: false,
            force_hls_relay: false,
        });
        let stream = StreamLink {
            url: "https://media/a.m3u8".into(),
            resolution: "1080p".into(),
            hls: true,
            provider: "Default".into(),
            downloadable: true,
            headers: RequestHeaders::default(),
            subtitles: vec![],
        };

        assert_eq!(
            &player.command_args_inner(&stream, "Anime", true)[..3],
            ["--no-stdin", "--keep-running", "https://media/a.m3u8"]
        );
    }

    #[test]
    fn android_mpv_arguments_use_an_explicit_view_intent() {
        let player = Player::new(PlayerOptions {
            executable: "termux-am-starter".into(),
            kind: PlayerKind::AndroidMpv,
            no_detach: true,
            exit_after_play: false,
            force_hls_relay: false,
        });
        let stream = StreamLink {
            url: "http://127.0.0.1:43123/stream-token".into(),
            resolution: "1080p".into(),
            hls: true,
            provider: "Anikoto".into(),
            downloadable: true,
            headers: RequestHeaders::default(),
            subtitles: vec![],
        };

        assert_eq!(
            player.command_args(&stream, "Anime Episode 1"),
            vec![
                "start",
                "--user",
                "0",
                "-a",
                "android.intent.action.VIEW",
                "-d",
                "http://127.0.0.1:43123/stream-token",
                "-n",
                "is.xyz.mpv/.MPVActivity",
                "--es",
                "title",
                "Anime Episode 1",
            ]
        );
    }

    #[test]
    fn android_vlc_arguments_target_the_android_app_not_terminal_vlc() {
        let player = Player::new(PlayerOptions {
            executable: "am".into(),
            kind: PlayerKind::AndroidVlc,
            no_detach: true,
            exit_after_play: false,
            force_hls_relay: false,
        });
        let stream = StreamLink {
            url: "https://media.example/episode.m3u8".into(),
            resolution: "720p".into(),
            hls: true,
            provider: "Anikoto".into(),
            downloadable: true,
            headers: RequestHeaders::default(),
            subtitles: vec![],
        };

        assert!(
            player.command_args(&stream, "Episode").contains(
                &"org.videolan.vlc/org.videolan.vlc.gui.video.VideoPlayerActivity".into()
            )
        );
    }

    #[test]
    fn android_launcher_lookup_prefers_the_first_available_candidate() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("termux-am"), "").unwrap();
        let path = std::env::join_paths([directory.path()]).unwrap();

        assert_eq!(
            find_in_path("termux-am", &path),
            Some(directory.path().join("termux-am"))
        );
        assert_eq!(find_in_path("termux-am-starter", &path), None);
    }

    #[test]
    fn only_termux_activity_launchers_allow_the_url_opener_fallback() {
        assert!(is_termux_activity_launcher(Path::new("termux-am-starter")));
        assert!(is_termux_activity_launcher(Path::new(
            "/data/data/com.termux/files/usr/bin/termux-am"
        )));
        assert!(is_termux_activity_launcher(Path::new("am")));
        assert!(!is_termux_activity_launcher(Path::new(
            "/data/local/tmp/custom-launcher"
        )));
    }

    #[test]
    fn android_fallback_uses_specific_media_types() {
        assert_eq!(android_media_type(true), "application/vnd.apple.mpegurl");
        assert_eq!(android_media_type(false), "video/mp4");
    }

    #[test]
    fn platform_default_selects_expected_player() {
        let options = PlayerOptions::default_player();
        if cfg!(target_os = "android") {
            assert_eq!(options.kind, PlayerKind::AndroidMpv);
        } else if cfg!(target_os = "macos") {
            assert_eq!(options.kind, PlayerKind::Iina);
            if std::env::var_os("ANI_CLI_PLAYER").is_none() {
                assert_eq!(options.executable, PathBuf::from("iina"));
            }
        } else {
            assert_eq!(options.kind, PlayerKind::Mpv);
        }
    }
}
