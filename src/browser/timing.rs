use std::time::Duration;

/// Per-stage durations of the last completed pipeline run (PLAN.md §4). At M1
/// the pipeline has exactly two timed stages; later milestones add a field per
/// stage here (parse at M2, style at M4, layout at M5, …) rather than
/// reshaping the struct.
#[derive(Default, Debug)]
pub struct Timings {
    /// The whole request — client build → last body byte — measured on the
    /// fetch worker and shipped as `Msg::Loaded::elapsed`.
    pub fetch: Option<Duration>,
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
        [("fetch", self.fetch), ("frame", self.frame)]
            .into_iter()
            .filter_map(|(label, dur)| dur.map(|d| format!("{label} {}", format_ms(d))))
            .collect()
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
            frame: None,
        };
        assert_eq!(fetch_only.rows(), ["fetch 12.3 ms"]);

        let frame_only = Timings {
            fetch: None,
            frame: Some(Duration::from_micros(2_100)),
        };
        assert_eq!(frame_only.rows(), ["frame 2.1 ms"]);
    }

    #[test]
    fn rows_come_in_pipeline_order() {
        let both = Timings {
            fetch: Some(Duration::from_millis(40)),
            frame: Some(Duration::from_micros(2_100)),
        };
        assert_eq!(both.rows(), ["fetch 40.0 ms", "frame 2.1 ms"]);
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
