use std::io::Write;

use crate::config::remux_home;

pub fn run() -> anyhow::Result<()> {
    let home = remux_home();

    print!(
        "This will remove {} and all its contents.\nContinue? [y/N]: ",
        home.display()
    );
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().to_lowercase();

    if trimmed != "y" && trimmed != "yes" {
        println!("Cancelled.");
        return Ok(());
    }

    // Remove the PATH entry from shell rc files
    remove_shell_entries();

    // Remove ~/.remux
    if home.exists() {
        std::fs::remove_dir_all(&home)?;
        println!("Removed {}", home.display());
    }

    println!("\nremux uninstalled. Restart your shell to update PATH.");
    Ok(())
}

fn remove_shell_entries() {
    let home_dir = std::env::var("HOME").unwrap_or_default();
    if home_dir.is_empty() {
        return;
    }

    let rc_files = [
        format!("{}/.zshrc", home_dir),
        format!("{}/.bashrc", home_dir),
        format!("{}/.bash_profile", home_dir),
        format!("{}/.profile", home_dir),
        format!("{}/.config/fish/config.fish", home_dir),
    ];

    for path in &rc_files {
        if let Ok(contents) = std::fs::read_to_string(path) {
            let cleaned = remove_remux_block(&contents);
            if cleaned != contents {
                if std::fs::write(path, &cleaned).is_ok() {
                    println!("Cleaned PATH entry from {}", path);
                }
            }
        }
    }
}

/// Remove the `# remux` block (comment + export line) from rc file contents.
fn remove_remux_block(contents: &str) -> String {
    let mut result = Vec::new();
    let mut lines = contents.lines().peekable();

    while let Some(line) = lines.next() {
        if line.trim() == "# remux" {
            // Skip the comment and the next non-empty line (the export/set line)
            while let Some(next) = lines.peek() {
                if next.trim().is_empty() {
                    lines.next();
                } else if next.contains(".remux") {
                    lines.next();
                    break;
                } else {
                    break;
                }
            }
            // Remove trailing blank line left behind
            if result.last().map(|l: &&str| l.is_empty()).unwrap_or(false) {
                result.pop();
            }
        } else {
            result.push(line);
        }
    }

    // Preserve trailing newline if original had one
    let mut out = result.join("\n");
    if contents.ends_with('\n') {
        out.push('\n');
    }
    out
}
