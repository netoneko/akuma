//! Git log functionality
//!
//! Implements viewing commit history.

use alloc::string::String;

use libakuma::print;

use crate::error::Result;
use crate::refs::RefManager;
use crate::sha1;
use crate::store::ObjectStore;

/// Show commit log starting from HEAD
pub fn show_log(max_commits: Option<usize>, oneline: bool) -> Result<()> {
    let git_dir = crate::git_dir();
    let store = ObjectStore::new(&git_dir);
    let refs = RefManager::new(&git_dir);

    // Start from HEAD
    let mut current_sha = refs.resolve_head()?;
    let mut count = 0;

    loop {
        // Check if we've reached the limit
        if let Some(max) = max_commits {
            if count >= max {
                break;
            }
        }

        // Read commit object
        let obj = store.read(&current_sha)?;
        let commit = obj.as_commit()?;

        if oneline {
            // Compact format: abc123d Message first line
            let short_sha = &sha1::to_hex(&current_sha)[..7];
            let first_line = commit.message.lines().next().unwrap_or("");
            print(short_sha);
            print(" ");
            print(first_line);
            print("\n");
        } else {
            // Full format
            print("commit ");
            print(&sha1::to_hex(&current_sha));
            print("\n");

            // Parse and display author
            let (name, email, timestamp, tz) = parse_author_line(&commit.author);
            print("Author: ");
            print(name);
            print(" <");
            print(email);
            print(">\n");

            print("Date:   ");
            print(&format_date(timestamp, tz));
            print("\n");

            print("\n");
            // Indent message
            for line in commit.message.lines() {
                print("    ");
                print(line);
                print("\n");
            }
            print("\n");
        }

        count += 1;

        // Move to parent commit
        if commit.parents.is_empty() {
            break;
        }
        current_sha = commit.parents[0];
    }

    Ok(())
}

/// Parse an author/committer line: "Name <email> timestamp timezone"
fn parse_author_line(line: &str) -> (&str, &str, i64, &str) {
    // Format: "Name <email> timestamp timezone"
    // Example: "John Doe <john@example.com> 1706540000 +0000"
    
    // Find email start and end
    let email_start = line.find('<').unwrap_or(0);
    let email_end = line.find('>').unwrap_or(line.len());
    
    let name = line[..email_start].trim();
    let email = if email_start < email_end {
        &line[email_start + 1..email_end]
    } else {
        ""
    };

    // Parse timestamp and timezone after the email
    let after_email = if email_end + 1 < line.len() {
        line[email_end + 1..].trim()
    } else {
        ""
    };

    let mut parts = after_email.split_whitespace();
    let timestamp: i64 = parts.next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let tz = parts.next().unwrap_or("+0000");

    (name, email, timestamp, tz)
}

/// Format a Unix timestamp as a human-readable date
fn format_date(timestamp: i64, tz: &str) -> String {
    // Simple date formatting without a full datetime library
    // We'll compute the date components manually
    
    // Days since Unix epoch (Jan 1, 1970)
    let days = timestamp / 86400;
    let time_of_day = timestamp % 86400;
    
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Calculate year, month, day using a simplified algorithm
    let (year, month, day) = days_to_ymd(days);
    
    // Day of week (Jan 1, 1970 was Thursday = 4)
    let weekday = ((days % 7) + 4) % 7;
    let weekday_name = match weekday {
        0 => "Sun",
        1 => "Mon",
        2 => "Tue",
        3 => "Wed",
        4 => "Thu",
        5 => "Fri",
        6 => "Sat",
        _ => "???",
    };

    let month_name = match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "???",
    };

    alloc::format!(
        "{} {} {} {:02}:{:02}:{:02} {} {}",
        weekday_name,
        month_name,
        day,
        hours,
        minutes,
        seconds,
        year,
        tz
    )
}

/// Convert days since Unix epoch to (year, month, day)
fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    // Algorithm based on Howard Hinnant's date algorithms
    // http://howardhinnant.github.io/date_algorithms.html
    
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    
    (y, m, d)
}
