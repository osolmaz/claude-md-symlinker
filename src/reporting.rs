use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Summary {
    pub repos_scanned: usize,
    pub created: usize,
    pub repaired: usize,
    pub refreshed: usize,
    pub kept: usize,
    pub no_source: usize,
    pub conflicts: usize,
    pub tracked_conflicts: usize,
    pub exclude_updates: usize,
    pub cleaned: usize,
    pub errors: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub summary: Summary,
    pub results: Vec<RepoResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoResult {
    pub repo: String,
    pub adapter: String,
    pub source: String,
    pub target: String,
    pub status: Status,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Created,
    Repaired,
    Refreshed,
    Kept,
    NoSource,
    Conflict,
    TrackedConflict,
    ExcludeUpdated,
    Cleaned,
    Error,
}

impl Report {
    pub fn new(repos_scanned: usize) -> Self {
        Self {
            summary: Summary {
                repos_scanned,
                ..Summary::default()
            },
            results: Vec::new(),
        }
    }

    pub fn push(&mut self, result: RepoResult) {
        match result.status {
            Status::Created => self.summary.created += 1,
            Status::Repaired => self.summary.repaired += 1,
            Status::Refreshed => self.summary.refreshed += 1,
            Status::Kept => self.summary.kept += 1,
            Status::NoSource => self.summary.no_source += 1,
            Status::Conflict => self.summary.conflicts += 1,
            Status::TrackedConflict => self.summary.tracked_conflicts += 1,
            Status::ExcludeUpdated => self.summary.exclude_updates += 1,
            Status::Cleaned => self.summary.cleaned += 1,
            Status::Error => self.summary.errors += 1,
        }
        self.results.push(result);
    }

    pub fn exit_code(&self) -> u8 {
        if self.summary.errors > 0 {
            1
        } else if self.summary.conflicts > 0 || self.summary.tracked_conflicts > 0 {
            2
        } else {
            0
        }
    }
}

pub fn print_plain(report: &Report, dry_run: bool) {
    if dry_run {
        println!("Dry run. No filesystem changes were made.");
    }

    println!("Scanned {} repositories.", report.summary.repos_scanned);
    println!("Created {} shims.", report.summary.created);
    println!("Repaired {} shims.", report.summary.repaired);
    println!("Refreshed {} copies.", report.summary.refreshed);
    println!("Kept {} managed shims.", report.summary.kept);
    println!(
        "Skipped {} repos without sources.",
        report.summary.no_source
    );
    println!("Skipped {} conflicts.", report.summary.conflicts);
    println!(
        "Skipped {} tracked targets.",
        report.summary.tracked_conflicts
    );
    println!("Updated {} exclude files.", report.summary.exclude_updates);
    println!("Cleaned {} shims.", report.summary.cleaned);
    println!("Errors {}.", report.summary.errors);

    for result in &report.results {
        if matches!(
            result.status,
            Status::Conflict | Status::TrackedConflict | Status::Error
        ) {
            println!(
                "{}: {} {} -> {} ({})",
                result.repo, result.adapter, result.source, result.target, result.message
            );
        }
    }
}

pub fn print_json(report: &Report) {
    println!(
        "{}",
        serde_json::to_string_pretty(report).expect("report serialization should not fail")
    );
}
