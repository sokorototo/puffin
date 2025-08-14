use egui::TextBuffer;
use puffin::*;

use crate::filter::Filter;

#[derive(Clone, Debug, Default)]
pub struct Options {
    filter: Filter,
}

pub fn ui(
    ui: &mut egui::Ui,
    options: &mut Options,
    scope_infos: &ScopeCollection,
    frames: &[std::sync::Arc<UnpackedFrameData>],
) {
    let mut threads = std::collections::HashSet::<&ThreadInfo>::new();
    let mut stats = Stats::default();

    for frame in frames {
        threads.extend(frame.thread_streams.keys());
        for stream in frame.thread_streams.values() {
            collect_stream(&mut stats, &stream.stream).ok();
        }
    }

    let mut total_bytes = 0;
    let mut total_ns = 0;
    for scope in stats.scopes.values() {
        total_bytes += scope.iter().map(|s| s.bytes).sum::<usize>();
        total_ns += scope.iter().map(|s| s.self_time).sum::<i64>();
    }

    ui.label("This view can be used to find functions that are called a lot.\n\
              The overhead of a profile scope is around ~50ns, so remove profile scopes from fast functions that are called often.");

    ui.label(format!(
        "Currently viewing {} unique scopes, using a total of {:.1} kB, covering {:.1} ms over {} thread(s)",
        stats.scopes.len(),
        total_bytes as f32 * 1e-3,
        total_ns as f32 * 1e-6,
        threads.len()
    ));

    options.filter.ui(ui);

    let mut scopes: Vec<_> = stats
        .scopes
        .into_iter()
        .map(|(key, value)| (key, value))
        .collect();

    scopes.sort_by_key(|(key, _)| key.clone());
    scopes.sort_by_key(|(_key, scope_stats)| scope_stats.len());
    scopes.reverse();

    egui::ScrollArea::horizontal().show(ui, |ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        ui.spacing_mut().item_spacing.x = 16.0;

        egui_extras::TableBuilder::new(ui)
            .striped(true)
            .columns(
                egui_extras::Column::auto_with_initial_suggestion(200.0).resizable(true),
                3,
            )
            .columns(egui_extras::Column::auto().resizable(false), 6)
            .header(20.0, |mut header| {
                header.col(|ui| {
                    ui.strong("Location");
                });
                header.col(|ui| {
                    ui.strong("Function Name");
                });
                header.col(|ui| {
                    ui.strong("Scope Name");
                });
                header.col(|ui| {
                    ui.strong("Count");
                });
                header.col(|ui| {
                    ui.strong("Total self time");
                });
                header.col(|ui| {
                    ui.strong("Mean self time");
                });
                header.col(|ui| {
                    ui.strong("Max self time");
                });
                header.col(|ui| {
                    ui.strong("Variance");
                });
                header.col(|ui| {
                    ui.strong("P99");
                });
            })
            .body(|mut body| {
                for (key, stats) in &scopes {
                    // calculate measures of central tendency
                    let Some((count, mean, population_variance, ..)) =
                        welford_variance(stats.iter().map(|s| s.self_time))
                    else {
                        return;
                    };

                    let percentile_99 =
                        percentile_interpolated(stats.iter().map(|s| s.self_time), 0.99)
                            .unwrap_or(0.0);

                    let Some(scope_details) = scope_infos.fetch_by_id(&key.id) else {
                        continue;
                    };

                    if !options.filter.is_empty() {
                        let mut matches = options.filter.include(&scope_details.function_name);

                        if let Some(scope_name) = &scope_details.scope_name {
                            matches |= options.filter.include(scope_name);
                        }

                        if !matches {
                            continue;
                        }
                    }

                    body.row(14.0, |mut row| {
                        row.col(|ui| {
                            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                            ui.label(scope_details.location());
                        });
                        row.col(|ui| {
                            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                            ui.label(scope_details.function_name.as_str());
                        });

                        row.col(|ui| {
                            if let Some(name) = &scope_details.scope_name {
                                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                                ui.label(name.as_ref());
                            }
                        });
                        row.col(|ui| {
                            let color = if stats.len() < 1_000 {
                                ui.visuals().text_color()
                            } else if stats.len() < 10_000 {
                                ui.visuals().warn_fg_color
                            } else {
                                ui.visuals().error_fg_color
                            };

                            ui.label(
                                egui::RichText::new(format!("{:>5}", count))
                                    .monospace()
                                    .color(color),
                            );
                        });

                        row.col(|ui| {
                            ui.monospace(format!("{:>8.1} µs", (count as f64) * mean * 1e-3));
                        });
                        row.col(|ui| {
                            ui.monospace(format!("{:>8.1} µs", mean));
                        });
                        row.col(|ui| {
                            ui.monospace(format!(
                                "{:>8.1} µs",
                                stats.iter().map(|s| s.self_time).max().unwrap_or(0) as f32 * 1e-3
                            ));
                        });

                        row.col(|ui| {
                            ui.monospace(format!("{:>8.1}", population_variance));
                        });
                        row.col(|ui| {
                            ui.monospace(format!("{:>8.1}", percentile_99));
                        });
                    });
                }
            });
    });
}

#[derive(Default)]
struct Stats {
    scopes: std::collections::HashMap<Key, Vec<ScopeStats>>,
}

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    id: ScopeId,
}

#[derive(Copy, Clone, Default)]
struct ScopeStats {
    bytes: usize,
    self_time: NanoSecond,
}

fn collect_stream(stats: &mut Stats, stream: &puffin::Stream) -> puffin::Result<()> {
    for scope in puffin::Reader::from_start(stream) {
        collect_scope(stats, stream, &scope?)?;
    }
    Ok(())
}

fn collect_scope<'s>(
    stats: &mut Stats,
    stream: &'s puffin::Stream,
    scope: &puffin::Scope<'s>,
) -> puffin::Result<()> {
    let mut ns_used_by_children = 0;
    for child_scope in Reader::with_offset(stream, scope.child_begin_position)? {
        let child_scope = &child_scope?;
        collect_scope(stats, stream, child_scope)?;
        ns_used_by_children += child_scope.record.duration_ns;
    }

    let self_time = scope.record.duration_ns.saturating_sub(ns_used_by_children);

    let key = Key { id: scope.id };
    let scope_stats = stats.scopes.entry(key).or_default();
    scope_stats.push(ScopeStats {
        bytes: scope_byte_size(scope),
        self_time,
    });

    Ok(())
}

fn scope_byte_size(scope: &puffin::Scope<'_>) -> usize {
    1 + // `(` sentinel
    8 + // start time
    8 + // scope id
    1 + scope.record.data.len() + // dynamic data len
    8 + // scope size
    1 + // `)` sentinel
    8 // stop time
}

pub fn welford_variance<T: Iterator<Item = i64>>(data: T) -> Option<(usize, f64, f64, f64)> {
    let mut n: usize = 0;
    let mut mean: f64 = 0.0;
    let mut m2: f64 = 0.0;

    for x in data {
        n += 1;
        let delta = (x as f64) - mean;
        mean += delta / n as f64;
        let delta2 = (x as f64) - mean;
        m2 += delta * delta2;
    }

    if n == 0 {
        return None;
    }

    let pop_var = m2 / n as f64;
    let sample_var = if n > 1 {
        m2 / (n as f64 - 1.0)
    } else {
        f64::NAN
    };

    Some((n, mean, pop_var, sample_var))
}

pub fn percentile_interpolated<T: Iterator<Item = i64>>(data: T, p: f64) -> Option<f64> {
    if !(0.0..=1.0).contains(&p) {
        return None;
    }
    let mut v: Vec<f64> = data.map(|x| x as f64).collect();
    if v.is_empty() {
        return None;
    }

    v.sort_by(|a, b| a.total_cmp(b));

    let n = v.len();
    if n == 1 {
        return Some(v[0]);
    }

    // position in [0, n-1]
    let pos = p * (n as f64 - 1.0);
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        Some(v[lo])
    } else {
        let frac = pos - lo as f64;
        Some(v[lo] + frac * (v[hi] - v[lo]))
    }
}
