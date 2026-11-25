//! Options → Backup's progress window: a small dialog with a bar that fills as the
//! backup (or its restore) goes, the step it is on, and the result line at the end
//! (the user's ask: the process seen graphically). The work runs on a worker thread
//! (backup.rs's `Job`), this window only draws what it reports.

use crate::backup::{Job, Step};
use egui::{Color32, Rect, Vec2};

pub struct Dialog {
    job: Job,
    /// Whether the job restores (the title and the end line say so) or exports.
    restore: bool,
    /// The last step drawn, to repaint on each change even when nothing else does.
    seen: Step,
    /// A finished restore's reload was asked once (the list is re-read from the disk).
    said: bool,
}

impl Dialog {
    pub fn new(job: Job) -> Self {
        Self {
            job,
            restore: false,
            seen: Step::Waiting,
            said: false,
        }
    }

    #[allow(dead_code)] // with_restore is the one the Outcome::Done path takes
    pub fn restoring(mut self) -> Self {
        self.restore = true;
        self
    }

    /// Say on the window whether this one restores (`restore`) or exports.
    pub fn with_restore(mut self, restore: bool) -> Self {
        self.restore = restore;
        self
    }

    /// Draw it; answers (still open, reload once after a finished restore).
    pub fn show(&mut self, ctx: &egui::Context) -> (bool, bool) {
        let step = self.job.step();
        let changed = step != self.seen;
        if changed {
            self.seen = step.clone();
        }
        let mut open = true;
        let mut reload = false;
        let mut ok = false;
        let title = if self.restore {
            "Restore backup"
        } else {
            "Backup"
        };
        egui::Window::new(title)
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let (fraction, line) = match &step {
                    Step::Waiting => (0.0, "Preparing…".to_string()),
                    Step::Work { done, total, what } => {
                        let f = if *total > 0 {
                            *done as f32 / *total as f32
                        } else {
                            0.2
                        };
                        let what = match *what {
                            "read" => "Reading the backup…".to_string(),
                            "write" => "Writing the backup file…".to_string(),
                            _ => {
                                if *total > 0 {
                                    format!("Copying part files… {done} of {total}")
                                } else {
                                    "Copying part files…".to_string()
                                }
                            }
                        };
                        (f, what)
                    }
                    Step::Done(Ok(skipped)) => {
                        if self.restore && !self.said {
                            reload = true;
                            self.said = true;
                        }
                        (
                            1.0,
                            if skipped.is_empty() {
                                if self.restore {
                                    "Restored. Restart MDM to see everything as the backup \
                                     had it."
                                        .to_string()
                                } else {
                                    "Backup saved.".to_string()
                                }
                            } else {
                                format!("Finished, but:\n{skipped}")
                            },
                        )
                    }
                    Step::Done(Err(e)) => (1.0, format!("Failed: {e}")),
                };
                ui.set_min_width(320.0);
                ui.set_max_width(340.0);
                ui.add_space(6.0);
                // The line wraps at the window's width (a long path's skipped line reads whole,
                // not tangled over the bar).
                ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                    ui.label(egui::RichText::new(line).size(13.0));
                });
                ui.add_space(6.0);
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(320.0, 14.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 2.0, Color32::from_gray(230));
                ui.painter().rect_filled(
                    Rect::from_min_size(
                        rect.min,
                        Vec2::new(rect.width() * fraction.clamp(0.0, 1.0), 14.0),
                    ),
                    2.0,
                    Color32::from_rgb(0, 120, 215),
                );
                ui.add_space(6.0);
                let done = matches!(step, Step::Done(_));
                ui.add_space(2.0);
                if done
                    && ui
                        .add(egui::Button::new("OK").min_size(Vec2::new(75.0, 23.0)))
                        .clicked()
                {
                    // This window only: `Close` would take the app's own window down.
                    ok = true;
                }
            });
        if changed {
            ctx.request_repaint();
        }
        open &= !ok;
        (open, reload)
    }
}
