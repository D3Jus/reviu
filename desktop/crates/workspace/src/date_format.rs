pub(crate) fn format_relative_secs(updated_at_secs: u64, now_secs: u64) -> String {
  let delta = now_secs.saturating_sub(updated_at_secs);
  match delta {
    0..=59 => "now".to_string(),
    60..=3_599 => format!("{}m", delta / 60),
    3_600..=86_399 => format!("{}h", delta / 3_600),
    _ => format!("{}d", delta / 86_400),
  }
}

pub(crate) fn format_relative_age(updated_at_secs: u64, now_secs: u64) -> String {
  match format_relative_secs(updated_at_secs, now_secs).as_str() {
    "now" => "now".to_string(),
    label => format!("{label} ago"),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn format_relative_secs_buckets() {
    assert_eq!(format_relative_secs(100, 100), "now");
    assert_eq!(format_relative_secs(100, 159), "now");
    assert_eq!(format_relative_secs(100, 160), "1m");
    assert_eq!(format_relative_secs(100, 100 + 3_600), "1h");
    assert_eq!(format_relative_secs(100, 100 + 86_400), "1d");
    assert_eq!(format_relative_secs(100, 100 + 3 * 86_400), "3d");
  }

  #[test]
  fn format_relative_secs_clamps_future_timestamps() {
    assert_eq!(format_relative_secs(200, 100), "now");
  }

  #[test]
  fn format_relative_age_adds_context_to_elapsed_time() {
    assert_eq!(format_relative_age(100, 100), "now");
    assert_eq!(format_relative_age(100, 100 + 60), "1m ago");
  }
}
