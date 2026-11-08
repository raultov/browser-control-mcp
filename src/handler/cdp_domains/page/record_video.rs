//! `record_video` (Chrome 156+): records the tab into an MP4 with
//! `Page.startScreenRecording` / `Page.stopScreenRecording`, read from the
//! `IO` stream Chrome answers. Chrome keeps the recording's state (it
//! refuses a second start and a stop without a start), so the server keeps
//! none. WebDriver BiDi has no video recording: Firefox lacks the capability.

use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use base64::Engine;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::json;
use std::path::PathBuf;

#[macros::mcp_tool(
    name = "record_video",
    description = "Records an MP4 (AV1) video of the tab (Chrome 156+): action 'start' begins while other tools keep acting on the page; 'stop' writes it to 'path' (under the upload roots: the working and temp directories and --upload-root ones) or a new temp file and returns {path, bytes, duration_s}. One recording per tab; it ends when the tab closes. Chrome draws no frames of a hidden tab (background or minimized), so only what was on screen is recorded. Chrome only (not available on Firefox)."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct RecordVideoTool {
    /// Default: the default instance.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// 'start' or 'stop'.
    pub action: String,
    /// 'start': most frames per second, 1 to 60. Default: Chrome's.
    pub frame_rate: Option<u32>,
    /// 'start': widest frame, in pixels (16 to 7680). Default: the tab's.
    pub max_width: Option<u32>,
    /// 'start': tallest frame, in pixels (16 to 4320). Default: the tab's.
    pub max_height: Option<u32>,
    /// 'stop': absolute .mp4 path to write (overwritten if it exists).
    pub path: Option<String>,
}

/// Most reads in a row that give no data before the stream counts as stuck.
const MAX_EMPTY_READS: usize = 100;

/// Largest video read from Chrome: the stream is held in memory until it
/// is written.
const MAX_VIDEO: usize = 256 * 1024 * 1024;

impl RecordVideoTool {
    /// Checks the arguments before any browser work: what to do, and for
    /// 'stop' the file to write (`None`: a new temporary file).
    fn checked(&self, roots: &[PathBuf]) -> Result<Option<PathBuf>, String> {
        let range = |name: &str, value: Option<u32>, (low, high): (u32, u32)| match value {
            Some(v) if !(low..=high).contains(&v) => {
                Err(format!("'{name}' must be {low} to {high}, got {v}"))
            }
            _ => Ok(()),
        };
        match self.action.as_str() {
            "start" => {
                range("frame_rate", self.frame_rate, (1, 60))?;
                range("max_width", self.max_width, (16, 7680))?;
                range("max_height", self.max_height, (16, 4320))?;
                match self.path {
                    Some(_) => Err("'path' goes with action 'stop'".into()),
                    None => Ok(None),
                }
            }
            "stop" => {
                let start_only = [self.frame_rate, self.max_width, self.max_height];
                if start_only.iter().any(Option::is_some) {
                    return Err(
                        "frame_rate, max_width and max_height go with action 'start'".into(),
                    );
                }
                let path = self.path.as_deref();
                path.map(|p| super::save_pdf::output_path(p, roots, (".mp4", &[".mp4"])))
                    .transpose()
            }
            other => Err(format!("'action' must be 'start' or 'stop', got '{other}'")),
        }
    }

    /// `Page.startScreenRecording`'s parameters.
    fn start_params(&self) -> serde_json::Value {
        let mut params = serde_json::Map::new();
        let given = [
            ("frameRate", self.frame_rate),
            ("maxWidth", self.max_width),
            ("maxHeight", self.max_height),
        ];
        for (key, value) in given {
            if let Some(value) = value {
                params.insert(key.into(), json!(value));
            }
        }
        params.into()
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: RecordVideoTool = crate::handler::parse_args(params)?;
        let path = args
            .checked(&handler.upload_roots)
            .map_err(CallToolError::from_message)?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        match args.action.as_str() {
            "start" => {
                let method = "Page.startScreenRecording";
                command(&target, method, args.start_params()).await?;
                Ok(CallToolResult::text_content(vec![STARTED.into()]))
            }
            _ => {
                let reply = command(&target, "Page.stopScreenRecording", json!({})).await?;
                let stream = reply["stream"].as_str().unwrap_or_default();
                let video = read_video(&target, stream).await?;
                write_video(&video, path).await
            }
        }
    }
}

/// The reply to 'start'.
const STARTED: &str =
    "Recording the tab: call record_video with action 'stop' to end it and write the video.";

/// Sends `method`; its result, or an error worded for the agent.
async fn command(
    target: &CdpTarget,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, CallToolError> {
    let reply = target.send_raw_command(method, params).await;
    match reply {
        Ok(reply) => Ok(reply.result.unwrap_or_default()),
        Err(e) => Err(CallToolError::from_message(refused(
            method,
            &crate::handler::cdp_failure("CDP Error", &e),
        ))),
    }
}

/// What Chrome's refusal `error` of `method` means for the agent.
fn refused(method: &str, error: &str) -> String {
    if error.contains("wasn't found") {
        format!("record_video needs Chrome 156 or later ({method} is not known to this browser)")
    } else if error.contains("No active screen recording") {
        "No recording is running in this tab: call record_video with action 'start' first.".into()
    } else if error.contains("already active") {
        "A recording (or a screencast) is already running in this tab: end it with action 'stop' first.".into()
    } else {
        error.to_string()
    }
}

/// Reads Chrome's `IO` stream `handle` to the end (at most [`MAX_VIDEO`]
/// bytes), then closes it.
async fn read_video(target: &CdpTarget, handle: &str) -> Result<Vec<u8>, CallToolError> {
    let mut video = Vec::new();
    let mut empty = 0;
    let read = loop {
        let params = json!({"handle": handle, "size": 1 << 20});
        let chunk = match target.send_raw_command("IO.read", params).await {
            Ok(reply) => reply.result.unwrap_or_default(),
            Err(e) => break Err(crate::handler::cdp_failure("CDP Error", &e)),
        };
        let before = video.len();
        if let Err(e) = append_chunk(&mut video, &chunk) {
            break Err(e);
        }
        empty = if video.len() == before { empty + 1 } else { 0 };
        if empty > MAX_EMPTY_READS {
            break Err("Chrome's video stream stopped giving data before its end".to_string());
        }
        if chunk["eof"].as_bool() != Some(false) {
            break Ok(());
        }
    };
    let _ = target
        .send_raw_command("IO.close", json!({"handle": handle}))
        .await;
    read.map(|()| video).map_err(CallToolError::from_message)
}

/// Appends one `IO.read` `chunk` to `video`, refusing past [`MAX_VIDEO`].
fn append_chunk(video: &mut Vec<u8>, chunk: &serde_json::Value) -> Result<(), String> {
    let data = chunk["data"].as_str().unwrap_or_default();
    let bytes = match chunk["base64Encoded"].as_bool() {
        Some(true) => base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| format!("the video stream is not valid base64: {e}"))?,
        _ => data.as_bytes().to_vec(),
    };
    if video.len() + bytes.len() > MAX_VIDEO {
        return Err(format!(
            "the video is over {MAX_VIDEO} bytes: record shorter, or with a lower max_width, max_height or frame_rate"
        ));
    }
    video.extend_from_slice(&bytes);
    Ok(())
}

/// Writes `video` to `path` (or a new temporary file) and answers what was
/// written.
async fn write_video(video: &[u8], path: Option<PathBuf>) -> Result<CallToolResult, CallToolError> {
    if video.is_empty() {
        return Err(CallToolError::from_message(
            "Chrome returned an empty video (nothing was drawn while recording: was the tab hidden?)",
        ));
    }
    let temporary = path.is_none();
    let path = match path {
        Some(path) => path,
        None => temporary_path().map_err(CallToolError::from_message)?,
    };
    if let Err(e) = tokio::fs::write(&path, video).await {
        // No empty file of the server's own is left behind.
        if temporary {
            let _ = tokio::fs::remove_file(&path).await;
        }
        let shown = path.display();
        return Err(CallToolError::from_message(format!(
            "could not write '{shown}': {e}"
        )));
    }
    let shown = path.display().to_string();
    let duration = mp4_duration(video);
    let seconds = duration
        .map(|s| format!(" of {s:.1} s"))
        .unwrap_or_default();
    let first = format!(
        "Saved a {}-byte video{seconds} (MP4) to {shown}.",
        video.len()
    );
    let body = json!({"path": shown, "bytes": video.len(), "duration_s": duration});
    let body = serde_json::to_string_pretty(&body).unwrap_or_default();
    Ok(CallToolResult::text_content(vec![
        format!("{first}\n{body}").into(),
    ]))
}

/// A new file of the server's own (unique, readable by its user only) in
/// the temp directory, for a video written without a path.
fn temporary_path() -> Result<PathBuf, String> {
    tempfile::Builder::new()
        .prefix("browser-control-mcp-video-")
        .suffix(".mp4")
        .tempfile()
        .and_then(|file| file.keep().map_err(|e| e.error))
        .map(|(_, path)| path)
        .map_err(|e| format!("could not create a file for the video in the temp directory: {e}"))
}

/// The duration of an MP4, in seconds rounded to tenths. Chrome writes a
/// fragmented MP4, whose movie header holds no duration: its end is the
/// last fragment's decode time (`tfdt`) plus its samples' durations
/// (`trun`, else the default of `tfhd` or `trex`), in the track's timescale
/// (`mdhd`). A plain MP4's movie header (`mvhd`) is read otherwise.
fn mp4_duration(mp4: &[u8]) -> Option<f64> {
    let mut walk = Mp4Walk::default();
    walk.boxes(mp4, 0);
    let seconds = match (walk.timescale, walk.end) {
        (Some(scale), end) if scale > 0 && end > 0 => end as f64 / scale as f64,
        _ => walk.movie?,
    };
    Some((seconds * 10.0).round() / 10.0)
}

/// What [`mp4_duration`] gathers walking an MP4's boxes.
#[derive(Default)]
struct Mp4Walk {
    /// The first track's id (`tkhd`): only its fragments are read.
    track: Option<u64>,
    /// The track of the current fragment (`tfhd`).
    fragment_track: Option<u64>,
    /// The first track's timescale (`mdhd`).
    timescale: Option<u64>,
    /// Sample duration when neither `trun` nor `tfhd` gives one (`trex`).
    default_duration: u64,
    /// The current fragment's default sample duration (`tfhd`).
    fragment_default: Option<u64>,
    /// Where the next samples start: the fragment's decode time (`tfdt`),
    /// else the end of the previous samples.
    decode_time: u64,
    /// The latest end of a fragment, in the timescale.
    end: u64,
    /// The movie header's duration, in seconds, when not zero.
    movie: Option<f64>,
}

/// An MP4 box's type, header length and size (header included).
struct BoxHeader<'a> {
    kind: &'a [u8],
    header: usize,
    size: usize,
}

/// The header of the box at `at` in `data`: a size of 1 is given in 64
/// bits after the type, 0 runs to the end.
fn box_at(data: &[u8], at: usize) -> Option<BoxHeader<'_>> {
    let kind = data.get(at + 4..at + 8)?;
    let (header, size) = match number(data, at, 4)? {
        0 => (8, data.len() - at),
        1 => (16, usize::try_from(number(data, at + 8, 8)?).ok()?),
        size => (8, usize::try_from(size).ok()?),
    };
    (size >= header).then_some(BoxHeader { kind, header, size })
}

/// The big-endian number of `width` bytes at `at` in `data`.
fn number(data: &[u8], at: usize, width: usize) -> Option<u64> {
    let bytes = data.get(at..at.checked_add(width)?)?;
    Some(bytes.iter().fold(0u64, |n, b| (n << 8) | u64::from(*b)))
}

impl Mp4Walk {
    /// Walks the boxes of `data`, into containers (at most 8 deep).
    fn boxes(&mut self, data: &[u8], depth: usize) {
        let mut at = 0;
        while let Some(BoxHeader { kind, header, size }) = box_at(data, at) {
            let Some(body) = data.get(at + header..at.saturating_add(size)) else {
                return;
            };
            match kind {
                b"moov" | b"trak" | b"mdia" | b"mvex" | b"moof" | b"traf" if depth < 8 => {
                    self.fragment_default = None;
                    self.boxes(body, depth + 1);
                }
                _ => self.leaf(kind, body),
            }
            at += size;
        }
    }

    /// Reads the boxes [`mp4_duration`] needs (`body` after the header).
    fn leaf(&mut self, kind: &[u8], body: &[u8]) {
        let wide = body.first() == Some(&1);
        match kind {
            b"mvhd" | b"mdhd" => {
                let (scale, duration) = match wide {
                    true => (number(body, 20, 4), number(body, 24, 8)),
                    false => (number(body, 12, 4), number(body, 16, 4)),
                };
                if kind == b"mdhd" {
                    self.timescale = self.timescale.or(scale);
                } else if let (Some(scale), Some(duration)) = (scale, duration) {
                    self.movie =
                        (scale > 0 && duration > 0).then(|| duration as f64 / scale as f64);
                }
            }
            b"tkhd" => {
                let id = number(body, if wide { 20 } else { 12 }, 4);
                self.track = self.track.or(id);
            }
            b"trex" if number(body, 4, 4) == self.track => {
                self.default_duration = number(body, 12, 4).unwrap_or(0);
            }
            b"tfhd" => {
                self.fragment_track = number(body, 4, 4);
                self.fragment_default = tfhd_duration(body);
            }
            b"tfdt" => self.decode_time = number(body, 4, if wide { 8 } else { 4 }).unwrap_or(0),
            b"trun" if self.fragment_track == self.track => {
                let default = self.fragment_default.unwrap_or(self.default_duration);
                let end = self
                    .decode_time
                    .saturating_add(trun_duration(body, default));
                self.decode_time = end;
                self.end = self.end.max(end);
            }
            _ => {}
        }
    }
}

/// The default sample duration a `tfhd` box gives, if it gives one.
fn tfhd_duration(body: &[u8]) -> Option<u64> {
    let flags = number(body, 1, 3)?;
    if flags & 0x8 == 0 {
        return None;
    }
    let offset = 8 + if flags & 0x1 != 0 { 8 } else { 0 } + if flags & 0x2 != 0 { 4 } else { 0 };
    number(body, offset, 4)
}

/// The total duration of a `trun` box's samples, each `default` long
/// unless the box gives each one's.
fn trun_duration(body: &[u8], default: u64) -> u64 {
    let (Some(flags), Some(count)) = (number(body, 1, 3), number(body, 4, 4)) else {
        return 0;
    };
    if flags & 0x100 == 0 {
        return default.saturating_mul(count);
    }
    let mut at = 8 + if flags & 0x1 != 0 { 4 } else { 0 } + if flags & 0x4 != 0 { 4 } else { 0 };
    let per_sample = [0x100, 0x200, 0x400, 0x800]
        .iter()
        .filter(|bit| flags & **bit != 0)
        .count()
        * 4;
    let mut total = 0u64;
    for _ in 0..count {
        let Some(duration) = number(body, at, 4) else {
            break;
        };
        total = total.saturating_add(duration);
        at += per_sample;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::cdp_domains::tests::{call, mock_handler, spawn_recording_mock};

    fn tool(args: serde_json::Value) -> RecordVideoTool {
        serde_json::from_value(args).unwrap()
    }

    /// An MP4 movie header (version 0) of `duration` units at `timescale`.
    fn mvhd(timescale: u32, duration: u32) -> Vec<u8> {
        let mut mp4 = b"\0\0\0\x1cmvhd\0\0\0\0".to_vec();
        mp4.extend([0u8; 8]);
        mp4.extend(timescale.to_be_bytes());
        mp4.extend(duration.to_be_bytes());
        mp4
    }

    /// ISSUE-99: the arguments are checked before any browser work.
    #[test]
    fn arguments_are_checked_first() {
        let error = |args| tool(args).checked(&[]).unwrap_err();
        assert!(error(json!({"action": "pause"})).contains("'start' or 'stop'"));
        assert!(error(json!({"action": "start", "frame_rate": 0})).contains("1 to 60"));
        assert!(error(json!({"action": "start", "path": "/tmp/a.mp4"})).contains("'stop'"));
        assert!(error(json!({"action": "stop", "max_width": 640})).contains("'start'"));
        assert!(error(json!({"action": "stop", "path": "relative.mp4"})).contains("absolute"));
        // Absolute on every platform ("/tmp/..." is not on Windows)
        let webm = std::env::temp_dir().join("a.webm").display().to_string();
        assert!(error(json!({"action": "stop", "path": webm})).contains(".mp4"));
        assert_eq!(tool(json!({"action": "stop"})).checked(&[]), Ok(None));
    }

    /// ISSUE-99: Chrome's refusals are named for the agent.
    #[test]
    fn refusals_say_what_to_do() {
        let method = "Page.startScreenRecording";
        let old = refused(
            method,
            "CDP Error: 'Page.startScreenRecording' wasn't found",
        );
        assert!(old.contains("Chrome 156 or later"), "{old}");
        assert!(refused(method, "No active screen recording").contains("action 'start' first"));
        assert!(refused(method, "Screencast is already active").contains("action 'stop' first"));
    }

    /// ISSUE-99: the video's duration is read from its movie header.
    #[test]
    fn the_duration_is_read_from_the_movie_header() {
        assert_eq!(mp4_duration(&mvhd(1000, 2608)), Some(2.6));
        assert_eq!(mp4_duration(b"no header"), None);
        assert_eq!(mp4_duration(&mvhd(0, 5)), None);
    }

    /// ISSUE-99: Chrome's MP4 is fragmented (its movie header says 0): the
    /// duration is the last fragment's decode time plus its samples'.
    #[test]
    fn the_duration_of_a_fragmented_video_is_read_from_its_fragments() {
        let video = include_bytes!("../../../../tests/fixtures/screen-recording-156.mp4");
        assert_eq!(mp4_duration(video), Some(3.1));
    }

    /// An MP4 box of `kind` around `body`.
    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = u32::try_from(body.len() + 8).unwrap();
        [&size.to_be_bytes()[..], kind, body].concat()
    }

    /// Big-endian 32-bit fields.
    fn words(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    /// ISSUE-99: only the first track's fragments count, in its own
    /// timescale; a fragment without a decode time, or a second sample run,
    /// starts where the previous samples ended; a last box of size 0 (to
    /// the end) is read.
    #[test]
    fn fragments_of_the_first_track_are_timed_in_its_timescale() {
        let track = |id, scale| {
            let mdhd = bx(b"mdhd", &words(&[0, 0, 0, scale, 0]));
            [bx(b"tkhd", &words(&[0, 0, 0, id])), bx(b"mdia", &mdhd)].concat()
        };
        let moov = [
            bx(b"trak", &track(1, 1000)),
            bx(b"trak", &track(2, 48000)),
            bx(b"mvex", &bx(b"trex", &words(&[0, 1, 1, 100]))),
        ]
        .concat();
        // 10 samples of track 1 (100 each), twice, the second without tfdt
        let run = |id| [bx(b"tfhd", &words(&[0, id])), bx(b"trun", &words(&[0, 10]))].concat();
        let first = [
            bx(b"tfhd", &words(&[0, 1])),
            bx(b"tfdt", &words(&[0, 0])),
            bx(b"trun", &words(&[0, 10])),
        ]
        .concat();
        // A long sample of track 2, which does not count
        let other = [
            bx(b"tfhd", &words(&[0, 2])),
            bx(b"trun", &words(&[0x100, 1, 480_000])),
        ]
        .concat();
        let mp4 = [
            bx(b"moov", &moov),
            bx(b"moof", &bx(b"traf", &first)),
            bx(b"moof", &bx(b"traf", &run(1))),
            bx(b"moof", &bx(b"traf", &other)),
            b"\0\0\0\0mdat-to-the-end".to_vec(),
        ]
        .concat();
        assert_eq!(mp4_duration(&mp4), Some(2.0));
    }

    /// ISSUE-99: a stream that keeps answering nothing without ending is
    /// given up, closed, and said so.
    #[tokio::test]
    async fn a_stream_that_never_ends_is_given_up() {
        let mock = spawn_recording_mock().await;
        mock.respond_with(
            "Page.stopScreenRecording",
            |_| json!({"result": {"stream": "9"}}),
        );
        mock.respond_with(
            "IO.read",
            |_| json!({"result": {"data": "", "base64Encoded": false, "eof": false}}),
        );
        let handler = mock_handler(mock.http_port);
        let stop = json!({"action": "stop"});
        let error = RecordVideoTool::handle(call("record_video", stop), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("stopped giving data"), "{error}");
        assert_eq!(mock.commands_named("IO.close").len(), 1);
    }

    /// ISSUE-99: videos written without a path get files of their own.
    #[test]
    fn videos_without_a_path_get_files_of_their_own() {
        let (one, two) = (temporary_path().unwrap(), temporary_path().unwrap());
        assert_ne!(one, two);
        assert!(one.extension().is_some_and(|e| e == "mp4"));
        let _ = (std::fs::remove_file(one), std::fs::remove_file(two));
    }

    /// ISSUE-99: a chunk past the cap is refused, nothing more is kept.
    #[test]
    fn the_video_is_bounded() {
        let mut video = vec![0u8; MAX_VIDEO - 1];
        let chunk = json!({"data": "AAA=", "base64Encoded": true, "eof": false});
        assert!(
            append_chunk(&mut video, &chunk)
                .unwrap_err()
                .contains("over")
        );
        assert_eq!(video.len(), MAX_VIDEO - 1);
    }

    /// ISSUE-99: 'start' sends the options given; 'stop' reads the stream
    /// Chrome answers to the end, closes it and writes the MP4.
    #[tokio::test]
    async fn a_recording_is_started_then_written() {
        let mock = spawn_recording_mock().await;
        let mp4 = base64::engine::general_purpose::STANDARD.encode(mvhd(1000, 1500));
        mock.respond_with(
            "Page.startScreenRecording",
            |_| json!({"result": {"stream": "7"}}),
        );
        mock.respond_with(
            "Page.stopScreenRecording",
            |_| json!({"result": {"stream": "7"}}),
        );
        mock.respond_with(
            "IO.read",
            move |_| json!({"result": {"data": mp4.clone(), "base64Encoded": true, "eof": true}}),
        );
        let handler = mock_handler(mock.http_port);
        // When a recording is started at 10 fps
        let start = json!({"action": "start", "frame_rate": 10});
        let reply = RecordVideoTool::handle(call("record_video", start), &handler).await;
        assert!(reply.is_ok(), "{reply:?}");
        let sent = &mock.commands_named("Page.startScreenRecording")[0].params;
        assert_eq!(sent, &json!({"frameRate": 10}));
        // And stopped into a file
        let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let path = dir.path().join("v.mp4");
        let stop = json!({"action": "stop", "path": path.display().to_string()});
        let reply = RecordVideoTool::handle(call("record_video", stop), &handler)
            .await
            .unwrap();
        // Then the file holds the video and the reply says what it is
        let text = crate::handler::cdp_domains::tests::reply_text(&reply);
        assert!(text.contains("of 1.5 s (MP4)"), "{text}");
        assert_eq!(std::fs::read(&path).unwrap(), mvhd(1000, 1500));
        assert_eq!(mock.commands_named("IO.close")[0].params["handle"], "7");
    }
}
