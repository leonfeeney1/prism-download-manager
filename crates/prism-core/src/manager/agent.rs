//! Download agent: plain words in, a download started.
//!
//! Local and rule-based like the natural-language grabber: no AI model, no
//! API key, no network call to parse the sentence. The sentence is searched
//! (YouTube, via yt-dlp), the best match is probed for formats, the quality
//! the sentence asked for is picked, and the download starts — through the
//! same `confirm_video` path the picker window uses, so naming, categories
//! and settings behave identically.
//!
//! A pasted URL skips the search: a video URL is probed directly, a
//! playlist URL downloads its entries with the same choice.

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use super::capture::VideoDecision;
use super::jobs::{prepare_video_tools, wanted_container};
use super::{Core, StartMode};

/// What the agent understood from one sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRequest {
    /// Search words (everything that is not a command word or a quality).
    pub query: String,
    /// A pasted video/playlist URL, when the sentence carried one.
    pub url: Option<String>,
    /// Requested height (`1080p` → 1080, `4k` → 2160); None means best.
    pub height: Option<u32>,
    /// The sentence asked for audio only (`mp3`, `audio only` ...).
    pub audio_only: bool,
}

/// What the agent did.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentOutcome {
    /// The started item (first entry, for a playlist).
    pub item_id: String,
    pub title: String,
    pub url: String,
    /// Downloads started (1, or the playlist's entry count).
    pub count: usize,
    /// How many search results the pick was chosen from.
    pub candidates: usize,
}

/// Words that carry no meaning for the search: commands, fillers, the
/// quality itself (handled separately).
const FILLER: &[&str] = &[
    "download", "downloads", "downloading", "get", "gets", "fetch", "grab", "find", "finds",
    "please", "me", "my", "the", "a", "an", "and", "or", "of", "for", "to", "from", "on", "in",
    "at", "with", "as", "this", "that", "latest", "newest", "recent", "episode", "episodes", "ep", "video", "videos",
    "clip", "clips", "song", "songs", "music", "audio", "movie", "movies", "film", "films",
    "watch", "youtube", "yt", "hd", "full", "official",
];

/// Parse one sentence into an [`AgentRequest`]. Never starts anything.
pub fn parse_agent_request(text: &str) -> Result<AgentRequest> {
    let mut url = None;
    let mut kept: Vec<&str> = Vec::new();
    for token in text.split_whitespace() {
        let trimmed = token.trim_matches(|c: char| ".,;:!?()[]\"'".contains(c));
        if url.is_none()
            && (trimmed.starts_with("http://") || trimmed.starts_with("https://"))
        {
            url = Some(trimmed.to_string());
            continue;
        }
        kept.push(token);
    }
    let lower = kept.join(" ").to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();

    let mut height = None;
    for w in &words {
        let h = match *w {
            "8k" | "4320p" | "4320" => Some(4320),
            "4k" | "2160p" | "2160" => Some(2160),
            "1440p" | "1440" => Some(1440),
            "1080p" | "1080" => Some(1080),
            "720p" | "720" => Some(720),
            "480p" | "480" => Some(480),
            "360p" | "360" => Some(360),
            _ => None,
        };
        if h.is_some() {
            height = h;
        }
    }
    let audio_only = lower.contains("audio only")
        || lower.contains("audio-only")
        || lower.contains("as audio")
        || lower.contains("just the audio")
        || words.contains(&"mp3");

    let mut query_words: Vec<String> = Vec::new();
    for w in kept
        .iter()
        .map(|t| t.trim_matches(|c: char| ".,;:!?()[]\"'".contains(c)))
    {
        let lw = w.to_lowercase();
        if FILLER.contains(&lw.as_str()) {
            continue;
        }
        if ["8k", "4k", "4320p", "4320", "2160p", "2160", "1440p", "1440", "1080p", "1080", "720p", "720", "480p", "480", "360p", "360", "mp3"]
            .contains(&lw.as_str())
        {
            continue;
        }
        if w.is_empty() {
            continue;
        }
        query_words.push(w.to_string());
    }
    let query = query_words.join(" ");
    if url.is_none() && query.trim().is_empty() {
        return Err(anyhow!(
            "say what to download — for example \"the latest episode of Severance in 1080p\""
        ));
    }
    Ok(AgentRequest {
        query,
        url,
        height,
        audio_only,
    })
}

/// Score a search hit against the query: how many query words (longer than
/// two letters) appear in its title.
fn score_hit(query: &str, title: &str) -> usize {
    let title = title.to_lowercase();
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .filter(|w| title.contains(&w.to_lowercase()))
        .count()
}

impl Core {
    /// Run the download agent: parse the sentence, search, pick the best
    /// match, probe it, and start the download at the requested quality.
    pub async fn run_agent(&self, request: &str) -> Result<AgentOutcome> {
        let req = parse_agent_request(request)?;
        prepare_video_tools(self).await?;
        let settings = self.settings();

        // Resolve the sentence to one video (or a playlist's entries).
        let (url, title, candidates) = match &req.url {
            Some(url) => (url.clone(), None, 1),
            None => {
                let hits = self
                    .0
                    .video
                    .search(&req.query, 5)
                    .await
                    .context("searching for the video")?;
                if hits.is_empty() {
                    return Err(anyhow!("nothing found for \"{}\"", req.query));
                }
                let best = hits
                    .iter()
                    .enumerate()
                    .max_by_key(|(i, h)| (score_hit(&req.query, &h.title), std::cmp::Reverse(*i)))
                    .map(|(_, h)| h)
                    .expect("non-empty hits");
                (
                    best.url.clone(),
                    Some(best.title.clone()),
                    hits.len(),
                )
            }
        };

        let probe = self
            .0
            .video
            .probe(&url, &prism_video::RequestContext::default())
            .await
            .context("reading the video's formats")?;
        let (info, entries) = match probe {
            prism_video::Probe::Video(info) => (*info, Vec::new()),
            prism_video::Probe::Playlist(pl) => {
                // A playlist URL: the first entry carries the formats, the
                // rest follow with the same choice.
                let first = pl
                    .entries
                    .first()
                    .ok_or_else(|| anyhow!("that playlist is empty"))?;
                let first_probe = self
                    .0
                    .video
                    .probe(&first.url, &prism_video::RequestContext::default())
                    .await
                    .context("reading the playlist's first video")?;
                let info = match first_probe {
                    prism_video::Probe::Video(info) => *info,
                    prism_video::Probe::Playlist(_) => {
                        return Err(anyhow!("that link is a playlist of playlists"));
                    }
                };
                let entries: Vec<super::VideoEntry> = pl
                    .entries
                    .iter()
                    .take(100)
                    .map(|e| super::VideoEntry {
                        url: e.url.clone(),
                        title: e.title.clone(),
                        index: None,
                    })
                    .collect();
                (info, entries)
            }
        };

        let choices = self
            .0
            .video
            .choices(&info, wanted_container(None, &settings));
        let choice = pick_choice(&choices, &req)
            .ok_or_else(|| anyhow!("no downloadable format found for this video"))?;
        let decision = VideoDecision {
            capture_id: String::new(),
            page_url: url.clone(),
            choice: serde_json::to_value(choice).context("serializing the quality choice")?,
            options: serde_json::json!({}),
            save_dir: None,
            title: title.or(Some(info.title.clone())),
            thumbnail: info.thumbnail.clone(),
            duration: info.duration,
            entries,
            mode: StartMode::Now,
            context: None,
            one_at_a_time: false,
            section: None,
        };
        let count = if decision.entries.is_empty() {
            1
        } else {
            decision.entries.len()
        };
        let items = self.confirm_video(decision)?;
        let first = items
            .first()
            .ok_or_else(|| anyhow!("the download did not start"))?;
        Ok(AgentOutcome {
            item_id: first.id.clone(),
            title: info.title,
            url,
            count,
            candidates,
        })
    }
}

/// Pick the quality the sentence asked for: audio-only takes the best audio
/// choice; a height takes the best video choice at or below it (else the
/// smallest above it); no quality takes the best video choice.
fn pick_choice(
    choices: &[prism_video::Choice],
    req: &AgentRequest,
) -> Option<prism_video::Choice> {
    use prism_video::ChoiceKind;
    if req.audio_only {
        return choices
            .iter()
            .find(|c| c.kind == ChoiceKind::Audio)
            .cloned();
    }
    let videos: Vec<&prism_video::Choice> = choices
        .iter()
        .filter(|c| c.kind == ChoiceKind::Video)
        .collect();
    if videos.is_empty() {
        return choices.first().cloned();
    }
    match req.height {
        Some(want) => videos
            .iter()
            .filter(|c| c.height.unwrap_or(0) <= want)
            .max_by_key(|c| c.height.unwrap_or(0))
            .or_else(|| videos.iter().min_by_key(|c| c.height.unwrap_or(0)))
            .map(|c| (*c).clone()),
        None => videos.first().map(|c| (*c).clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quality_and_filler_words() {
        let r = parse_agent_request("get me the latest episode of Severance in 1080p").unwrap();
        assert_eq!(r.query, "Severance");
        assert_eq!(r.height, Some(1080));
        assert!(!r.audio_only);
        assert_eq!(r.url, None);
    }

    #[test]
    fn parses_audio_only_and_4k() {
        let r = parse_agent_request("download lofi beats as mp3 please").unwrap();
        assert!(r.audio_only);
        assert_eq!(r.query, "lofi beats");
        let r = parse_agent_request("fetch the trailer in 4k").unwrap();
        assert_eq!(r.height, Some(2160));
        assert_eq!(r.query, "trailer");
    }

    #[test]
    fn parses_a_pasted_url_and_keeps_the_words() {
        let r = parse_agent_request(
            "download this video https://www.youtube.com/watch?v=dQw4w9WgXcQ",
        )
        .unwrap();
        assert_eq!(
            r.url.as_deref(),
            Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
        );
        assert_eq!(r.query, "");
    }

    #[test]
    fn empty_request_fails() {
        assert!(parse_agent_request("download").is_err());
        assert!(parse_agent_request("  ").is_err());
    }

    #[test]
    fn scoring_prefers_the_title_match() {
        assert_eq!(score_hit("severance", "Severance — Season 2 Trailer"), 1);
        assert_eq!(score_hit("severance", "Unrelated Cooking Show"), 0);
        assert_eq!(score_hit("lofi beats to study", "Lofi Beats To Study To"), 3);
    }

    #[test]
    fn picks_the_requested_height() {
        use prism_video::{Choice, ChoiceKind};
        let mk = |h: u32| Choice {
            kind: ChoiceKind::Video,
            height: Some(h),
            ..Default::default()
        };
        let choices = vec![mk(2160), mk(1080), mk(720)];
        let req = AgentRequest {
            query: "x".into(),
            url: None,
            height: Some(1080),
            audio_only: false,
        };
        assert_eq!(pick_choice(&choices, &req).unwrap().height, Some(1080));
        let req = AgentRequest {
            height: Some(480),
            ..req
        };
        // Nothing at or below 480: the smallest above it.
        assert_eq!(pick_choice(&choices, &req).unwrap().height, Some(720));
        let req = AgentRequest {
            height: None,
            ..req
        };
        assert_eq!(pick_choice(&choices, &req).unwrap().height, Some(2160));
        let audio = Choice {
            kind: ChoiceKind::Audio,
            ..Default::default()
        };
        let mut with_audio = vec![audio];
        with_audio.extend(choices);
        let req = AgentRequest {
            height: None,
            audio_only: true,
            ..req
        };
        assert_eq!(
            pick_choice(&with_audio, &req).unwrap().kind,
            ChoiceKind::Audio
        );
    }
}
