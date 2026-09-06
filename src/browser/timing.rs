use std::time::Duration;

/// Per-stage durations of the last completed pipeline run (PLAN.md §4). Each
/// milestone adds a field per stage here (parse at M2, layout at M3, style at
/// M4, …) rather than reshaping the struct.
#[derive(Default, Debug)]
pub struct Timings {
    pub cache: Option<CacheOutcome>,
    /// The whole request — client build → last body byte — measured on the
    /// fetch worker and shipped as `Msg::Loaded::elapsed`.
    pub fetch: Option<Duration>,
    /// The HTML parse (tokenize + tree build), measured on the fetch worker
    /// and shipped as `Msg::Parsed::elapsed`.
    pub parse: Option<Duration>,
    /// DOM + stylesheets → computed values for every node. Runs on the UI
    /// thread like layout, and is the most expensive stage after parse on a
    /// large page (41 ms on the Wikipedia fixture, `perf.md`), so it needs to
    /// be visible in the instrument rather than only in a bench.
    pub style: Option<Duration>,
    /// Styled tree → display lines at the current column width. Unlike fetch
    /// and parse it runs on the UI thread, so `App` times it where it calls it.
    pub layout: Option<Duration>,
    /// The document-order `<script>` pass (M10.2): every script the page runs,
    /// measured on the UI thread where it runs, like style and layout. It sits
    /// after `layout` because that is when it happens — the pass is its own
    /// turn of the event loop, *after* the page has been painted, so that a
    /// script spending its whole budget cannot delay first paint. A page with
    /// no script still gets a row: the pass walked its tree to find that out,
    /// and that walk is real work the instrument should show.
    pub script: Option<Duration>,
    /// The last presented frame's draw + present time, recorded by the event
    /// loop after the fact.
    pub frame: Option<Duration>,
}

impl Timings {
    /// The formatted table: one `label N.N ms` row per stage that has run —
    /// a stage with no value yet has no row, not a placeholder. This is the
    /// single source of truth for timing output: the `F4` overlay draws
    /// exactly these rows and `--timing` prints exactly these rows.
    pub fn rows(&self) -> Vec<String> {
        let mut rows = Vec::new();
        if let Some(outcome) = self.cache {
            rows.push(format!("cache {}", outcome.label()));
        }
        rows.extend(
            [
                ("fetch", self.fetch),
                ("parse", self.parse),
                ("style", self.style),
                ("layout", self.layout),
                ("script", self.script),
                ("frame", self.frame),
            ]
            .into_iter()
            .filter_map(|(label, dur)| dur.map(|d| format!("{label} {}", format_ms(d)))),
        );
        rows
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheOutcome {
    Network,
    Hit,
    Revalidated,
}

impl CacheOutcome {
    fn label(self) -> &'static str {
        match self {
            CacheOutcome::Network => "network",
            CacheOutcome::Hit => "cache hit",
            CacheOutcome::Revalidated => "revalidated",
        }
    }
}

/// One-decimal milliseconds (`2.1 ms`) — the one duration format, shared by
/// the timing table and the statusline's frame-time segment.
pub fn format_ms(dur: Duration) -> String {
    format!("{:.1} ms", dur.as_secs_f64() * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_values_produce_no_rows() {
        assert!(
            Timings::default().rows().is_empty(),
            "a stage with no value has no row — no placeholders"
        );
    }

    #[test]
    fn each_stage_appears_only_once_it_has_a_value() {
        let fetch_only = Timings {
            fetch: Some(Duration::from_micros(12_300)),
            ..Timings::default()
        };
        assert_eq!(fetch_only.rows(), ["fetch 12.3 ms"]);

        let parse_only = Timings {
            parse: Some(Duration::from_micros(31_700)),
            ..Timings::default()
        };
        assert_eq!(parse_only.rows(), ["parse 31.7 ms"]);

        let style_only = Timings {
            style: Some(Duration::from_micros(41_100)),
            ..Timings::default()
        };
        assert_eq!(style_only.rows(), ["style 41.1 ms"]);

        let layout_only = Timings {
            layout: Some(Duration::from_micros(1_800)),
            ..Timings::default()
        };
        assert_eq!(layout_only.rows(), ["layout 1.8 ms"]);

        let script_only = Timings {
            script: Some(Duration::from_micros(700)),
            ..Timings::default()
        };
        assert_eq!(script_only.rows(), ["script 0.7 ms"]);

        let frame_only = Timings {
            frame: Some(Duration::from_micros(2_100)),
            ..Timings::default()
        };
        assert_eq!(frame_only.rows(), ["frame 2.1 ms"]);
    }

    #[test]
    fn rows_come_in_pipeline_order() {
        // Pipeline order, not struct order or alphabetical: fetch → parse →
        // style → layout → frame is the path a page actually takes.
        let all = Timings {
            cache: None,
            fetch: Some(Duration::from_millis(40)),
            parse: Some(Duration::from_micros(31_700)),
            style: Some(Duration::from_micros(41_100)),
            layout: Some(Duration::from_micros(1_800)),
            script: Some(Duration::from_micros(700)),
            frame: Some(Duration::from_micros(2_100)),
        };
        assert_eq!(
            all.rows(),
            [
                "fetch 40.0 ms",
                "parse 31.7 ms",
                "style 41.1 ms",
                "layout 1.8 ms",
                // After layout: the script pass is its own turn, and it runs
                // once the page the user is reading is already painted.
                "script 0.7 ms",
                "frame 2.1 ms"
            ]
        );
    }

    #[test]
    fn format_is_one_decimal_milliseconds() {
        assert_eq!(format_ms(Duration::ZERO), "0.0 ms");
        assert_eq!(format_ms(Duration::from_micros(2_100)), "2.1 ms");
        assert_eq!(format_ms(Duration::from_micros(2_149)), "2.1 ms");
        assert_eq!(format_ms(Duration::from_micros(50)), "0.1 ms");
        assert_eq!(format_ms(Duration::from_millis(1234)), "1234.0 ms");
    }
}
