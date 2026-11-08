//! Option normalization and ffmpeg command-line construction.

use std::path::Path;

pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mts", "m2ts", "flv", "m4v", "mkv", "mov", "webm", "avi", "wmv", "ts", "3gp", "mpg",
    "mpeg", "gif",
];

/// Legacy/non-MP4 containers that remux cleanly into .mp4
const REMUX_TO_MP4_EXTS: &[&str] = &[
    "mts", "m2ts", "flv", "webm", "m4v", "mov", "avi", "wmv", "ts", "3gp", "mpg", "mpeg", "gif",
];

/// Containers whose native audio codecs cannot be stream-copied into .mp4
const FORCE_AAC_TRANSCODE_EXTS: &[&str] = &["wmv", "avi", "flv", "mpg", "mpeg", "3gp", "webm"];

const ALLOWED_CRF: &[u32] = &[20, 23, 26];
const ALLOWED_PRESETS: &[&str] = &[
    "ultrafast", "superfast", "veryfast", "faster", "fast", "medium", "slow", "slower", "veryslow",
];
const ALLOWED_ENCODERS: &[&str] = &[
    "av1_nvenc",
    "hevc_nvenc",
    "h264_nvenc",
    "libx264",
];
const ALLOWED_CONCURRENCY: &[u32] = &[1, 2, 3, 4];

#[derive(Clone, Debug)]
pub struct Options {
    pub crf: u32,
    pub preset: String,
    pub encoder: String,
    pub concurrency: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            crf: 23,
            preset: "slow".into(),
            encoder: "av1_nvenc".into(),
            concurrency: 2,
        }
    }
}

/// Options as sent by the UI; anything missing or invalid falls back to the defaults.
#[derive(Debug, Default)]
pub struct RawOptions {
    pub crf: Option<f64>,
    pub preset: Option<String>,
    pub encoder: Option<String>,
    pub concurrency: Option<f64>,
}

impl Options {
    pub fn normalize(raw: RawOptions) -> Self {
        let d = Options::default();
        let crf = raw.crf.map(|v| v as u32).filter(|v| ALLOWED_CRF.contains(v));
        let concurrency = raw
            .concurrency
            .map(|v| v as u32)
            .filter(|v| ALLOWED_CONCURRENCY.contains(v));
        let preset = raw.preset.filter(|p| ALLOWED_PRESETS.contains(&p.as_str()));
        let encoder = raw.encoder.filter(|e| ALLOWED_ENCODERS.contains(&e.as_str()));
        Self {
            crf: crf.unwrap_or(d.crf),
            preset: preset.unwrap_or(d.preset),
            encoder: encoder.unwrap_or(d.encoder),
            concurrency: concurrency.unwrap_or(d.concurrency),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Primary,
    TranscodeAudio,
    CpuFallback,
}

pub fn ext_lower(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

pub fn is_gif(path: &Path) -> bool {
    ext_lower(path) == "gif"
}

pub fn needs_immediate_audio_transcode(path: &Path) -> bool {
    FORCE_AAC_TRANSCODE_EXTS.contains(&ext_lower(path).as_str())
}

pub fn is_video_file(path: &Path) -> bool {
    VIDEO_EXTENSIONS.contains(&ext_lower(path).as_str())
}

/// `<dir>/<stem>.batchviproc.w<id><ext>`, switching legacy containers to `.mp4`.
pub fn temp_name(input: &Path, worker_id: u32) -> std::path::PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let out_ext = if REMUX_TO_MP4_EXTS.contains(&ext_lower(input).as_str()) {
        ".mp4".to_string()
    } else {
        input
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default()
    };
    input.with_file_name(format!("{stem}.batchviproc.w{worker_id}{out_ext}"))
}

fn map_nvenc_preset(ui_preset: &str) -> &'static str {
    match ui_preset {
        "ultrafast" => "p4",
        "superfast" | "veryfast" => "p5",
        "faster" | "fast" => "p6",
        _ => "p7",
    }
}

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

pub fn build_ffmpeg_args(input: &Path, tmp: &Path, options: &Options, mode: Mode) -> Vec<String> {
    let input = input.to_string_lossy().into_owned();
    let tmp = tmp.to_string_lossy().into_owned();
    let crf = options.crf;
    let preset = options.preset.as_str();
    let nvenc_preset = map_nvenc_preset(preset);
    let gif = is_gif(Path::new(&input));

    // -reinit_filter 0 prevents mid-stream resolution changes from crashing the filtergraph
    let base = s(&[
        "-hide_banner", "-loglevel", "info", "-stats", "-fflags", "+genpts", "-reinit_filter", "0",
    ]);
    // Ensures dimensions are even and at least 2x2 (never 0x0 on tiny inputs)
    let even_scale = s(&["-vf", "scale=max(2\\,trunc(iw/2)*2):max(2\\,trunc(ih/2)*2)"]);
    let x264_audio = if gif {
        s(&["-an"])
    } else {
        s(&["-map", "0:a:0?", "-acodec", "aac", "-ac", "2", "-b:a", "128k"])
    };

    let mut args = base;

    // Software CPU fallback for tiny GIFs/stickers or streams rejected by NVENC.
    if mode == Mode::CpuFallback {
        args.extend(s(&["-i", &input, "-map", "0:V:0", "-map_metadata", "0", "-map_chapters", "-1"]));
        args.extend(even_scale);
        args.extend(s(&["-c:v", "libx264", "-preset", "slow", "-crf", &crf.to_string(), "-pix_fmt", "yuv420p"]));
        if gif {
            args.extend(s(&["-an"]));
        } else {
            // Same timing protections as the transcode-audio mode: old VFR WMV/AVI files
            // that reach this fallback would otherwise stall the muxer (FreeYourself.wmv).
            args.extend(s(&[
                "-fps_mode", "cfr", "-max_muxing_queue_size", "4096", "-map", "0:a:0?",
                "-af", "aresample=async=1:first_pts=0", "-c:a", "aac", "-ac", "2", "-b:a", "128k",
            ]));
        }
        args.extend(s(&["-movflags", "+faststart", "-y", &tmp]));
        return args;
    }

    let audio_flags = if gif {
        s(&["-an"])
    } else if mode == Mode::TranscodeAudio {
        s(&[
            "-fps_mode", "cfr", "-max_muxing_queue_size", "4096", "-map", "0:a:0?",
            "-map_chapters", "-1", "-af", "aresample=async=1:first_pts=0", "-c:a", "aac",
            "-ac", "2", "-b:a", "128k",
        ])
    } else {
        s(&["-map", "0:a?", "-c:a", "copy"])
    };

    let nvenc = |codec: &str, cq: u32, aq: &str, bf: &str, ten_bit: bool| {
        let mut a = s(&["-i", &input, "-map", "0:V:0", "-map_metadata", "0"]);
        a.extend(even_scale.clone());
        a.extend(s(&[
            "-c:v", codec, "-preset", nvenc_preset, "-tune", "hq", "-rc", "vbr", "-cq",
            &cq.to_string(), "-b:v", "0", "-multipass", "fullres", "-rc-lookahead", "32",
            "-spatial-aq", "1", "-temporal-aq", "1", "-aq-strength", aq, "-bf", bf,
            "-b_ref_mode", "middle",
        ]));
        if ten_bit {
            a.extend(s(&["-pix_fmt", "p010le"]));
        }
        a.extend(audio_flags.clone());
        a.extend(s(&["-movflags", "+faststart", "-y", &tmp]));
        a
    };

    match options.encoder.as_str() {
        "av1_nvenc" => {
            let cq = match crf { 20 => 32, 26 => 40, _ => 36 };
            args.extend(nvenc("av1_nvenc", cq, "5", "4", true));
        }
        "hevc_nvenc" => {
            let cq = match crf { 20 => 26, 26 => 34, _ => 30 };
            args.extend(nvenc("hevc_nvenc", cq, "6", "4", true));
        }
        "h264_nvenc" => {
            let cq = match crf { 20 => 22, 26 => 28, _ => 25 };
            args.extend(nvenc("h264_nvenc", cq, "7", "3", false));
        }
        _ => {
            args.extend(s(&["-i", &input, "-map", "0:V:0"]));
            args.extend(even_scale);
            args.extend(s(&["-vcodec", "libx264", "-preset", preset, "-crf", &crf.to_string()]));
            args.extend(s(&["-map_metadata", "0"]));
            args.extend(x264_audio);
            args.extend(s(&["-y", &tmp]));
        }
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn normalizes_invalid_options_to_defaults() {
        let o = Options::normalize(RawOptions {
            crf: Some(99.0),
            preset: Some("bogus".into()),
            encoder: Some("rm -rf".into()),
            concurrency: Some(9.0),
        });
        assert_eq!(o.crf, 23);
        assert_eq!(o.preset, "slow");
        assert_eq!(o.encoder, "av1_nvenc");
        assert_eq!(o.concurrency, 2);
    }

    #[test]
    fn temp_name_remuxes_legacy_containers() {
        let p = PathBuf::from(r"C:\v\clip.MOV");
        assert_eq!(temp_name(&p, 2), PathBuf::from(r"C:\v\clip.batchviproc.w2.mp4"));
        let p = PathBuf::from(r"C:\v\clip.mkv");
        assert_eq!(temp_name(&p, 1), PathBuf::from(r"C:\v\clip.batchviproc.w1.mkv"));
    }

    #[test]
    fn av1_args_match_electron_version() {
        let o = Options::default();
        let a = build_ffmpeg_args(Path::new("in.mp4"), Path::new("out.mp4"), &o, Mode::Primary);
        let joined = a.join(" ");
        assert!(joined.contains("-c:v av1_nvenc -preset p7 -tune hq -rc vbr -cq 36"));
        assert!(joined.contains("-aq-strength 5 -bf 4 -b_ref_mode middle -pix_fmt p010le"));
        assert!(joined.ends_with("-map 0:a? -c:a copy -movflags +faststart -y out.mp4"));
    }

    /// Writes args for every option combination to $ARGS_DUMP (used for parity checks
    /// against the old Electron implementation). Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "writes a parity dump; needs ARGS_DUMP"]
    fn dump_all_args() {
        let Ok(out) = std::env::var("ARGS_DUMP") else { return };
        let mut rows = Vec::new();
        for encoder in ALLOWED_ENCODERS {
            for crf in ALLOWED_CRF {
                for preset in ["slow", "medium", "veryfast"] {
                    for (mode_name, mode) in [
                        ("primary", Mode::Primary),
                        ("transcode-audio", Mode::TranscodeAudio),
                        ("cpu-fallback", Mode::CpuFallback),
                    ] {
                        for input in [r"C:\v\a.mp4", r"C:\v\b.gif", r"C:\v\c.wmv"] {
                            let o = Options {
                                crf: *crf,
                                preset: preset.into(),
                                encoder: encoder.to_string(),
                                concurrency: 2,
                            };
                            let tmp = temp_name(Path::new(input), 1);
                            rows.push(serde_json::json!({
                                "encoder": encoder, "crf": crf, "preset": preset, "mode": mode_name,
                                "input": input, "tmp": tmp.to_string_lossy(),
                                "args": build_ffmpeg_args(Path::new(input), &tmp, &o, mode),
                            }));
                        }
                    }
                }
            }
        }
        std::fs::write(out, serde_json::to_string(&rows).unwrap()).unwrap();
    }

    #[test]
    fn gif_drops_audio_and_cpu_fallback_uses_x264() {
        let o = Options::default();
        let a = build_ffmpeg_args(Path::new("a.gif"), Path::new("a.mp4"), &o, Mode::CpuFallback);
        let joined = a.join(" ");
        assert!(joined.contains("-c:v libx264 -preset slow -crf 23 -pix_fmt yuv420p -an"));
        assert!(!joined.contains("-fps_mode"), "GIFs keep their own frame timing");

        let v = build_ffmpeg_args(Path::new("a.wmv"), Path::new("a.mp4"), &o, Mode::CpuFallback).join(" ");
        assert!(v.contains("-fps_mode cfr -max_muxing_queue_size 4096"));
        assert!(v.contains("-af aresample=async=1:first_pts=0 -c:a aac"));
    }
}
