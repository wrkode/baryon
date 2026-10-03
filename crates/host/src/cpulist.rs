//! Parsing of the kernel CPU list format used by sysfs, for example
//! `0-3,8,10-11`.

/// Parse a kernel CPU list into a sorted, deduplicated list of CPU ids.
pub fn parse_cpu_list(s: &str) -> Result<Vec<u32>, String> {
    let s = s.trim();
    let mut cpus = Vec::new();
    if s.is_empty() {
        return Ok(cpus);
    }
    for part in s.split(',') {
        let part = part.trim();
        match part.split_once('-') {
            Some((start, end)) => {
                let start = parse_id(start, part)?;
                let end = parse_id(end, part)?;
                if end < start {
                    return Err(format!("descending range {part:?}"));
                }
                cpus.extend(start..=end);
            }
            None => cpus.push(parse_id(part, part)?),
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    Ok(cpus)
}

fn parse_id(s: &str, part: &str) -> Result<u32, String> {
    s.trim()
        .parse::<u32>()
        .map_err(|e| format!("invalid cpu id in {part:?}: {e}"))
}

/// Format CPU ids in the compact kernel list format.
pub fn format_cpu_list(cpus: &[u32]) -> String {
    let mut sorted = cpus.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = Vec::new();
    let mut iter = sorted.into_iter().peekable();
    while let Some(start) = iter.next() {
        let mut end = start;
        while iter.peek() == Some(&(end + 1)) {
            end += 1;
            iter.next();
        }
        if start == end {
            out.push(start.to_string());
        } else {
            out.push(format!("{start}-{end}"));
        }
    }
    out.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ranges_and_singles() {
        assert_eq!(
            parse_cpu_list("0-3,8,10-11").unwrap(),
            vec![0, 1, 2, 3, 8, 10, 11]
        );
        assert_eq!(parse_cpu_list("5").unwrap(), vec![5]);
        assert_eq!(parse_cpu_list("").unwrap(), Vec::<u32>::new());
        assert_eq!(parse_cpu_list("  0-1\n").unwrap(), vec![0, 1]);
    }

    #[test]
    fn rejects_invalid_input() {
        assert!(parse_cpu_list("3-1").is_err());
        assert!(parse_cpu_list("a").is_err());
        assert!(parse_cpu_list("0-").is_err());
    }

    #[test]
    fn formats_compactly() {
        assert_eq!(format_cpu_list(&[0, 1, 2, 3, 8, 10, 11]), "0-3,8,10-11");
        assert_eq!(format_cpu_list(&[3, 1, 2, 2]), "1-3");
        assert_eq!(format_cpu_list(&[]), "");
    }

    #[test]
    fn round_trips() {
        for s in ["0", "0-19", "0-3,8,10-11", "1,3,5"] {
            assert_eq!(format_cpu_list(&parse_cpu_list(s).unwrap()), s);
        }
    }
}
