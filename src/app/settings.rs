use anyhow::{Context, Result};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use webgis_core::{
    i18n::{self, Language},
    message,
};

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Theme {
    Light,
    Dark,
    #[default]
    System,
}

impl Theme {
    fn label(self) -> &'static str {
        match self {
            Self::Light => message!(light),
            Self::Dark => message!(dark),
            Self::System => message!(system),
        }
    }

    fn apply(self, ctx: &egui::Context) {
        ctx.set_theme(match self {
            Self::Light => egui::ThemePreference::Light,
            Self::Dark => egui::ThemePreference::Dark,
            Self::System => egui::ThemePreference::System,
        });
    }
}

#[derive(Clone, Copy, PartialEq, Serialize)]
struct Preferences {
    threads: usize,
    theme: Theme,
    #[serde(serialize_with = "serialize_language")]
    language: Language,
}

fn serialize_language<S: serde::Serializer>(
    language: &Language,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(language.code())
}

pub(super) fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("webgis-toolbox/settings.json"))
}

pub(crate) fn startup_language() -> Language {
    Preferences::load(settings_path().as_deref(), 1).language
}

impl Preferences {
    fn load(path: Option<&Path>, cpus: usize) -> Self {
        let stored: serde_json::Value = path
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let threads = stored["threads"]
            .as_u64()
            .filter(|&threads| threads > 0)
            .map_or((cpus / 2).clamp(1, 8), |threads| {
                threads.min(cpus as u64) as usize
            });
        let theme = Theme::deserialize(&stored["theme"]).unwrap_or_default();
        let language = stored["language"]
            .as_str()
            .map(Language::from_code)
            .unwrap_or_default();
        Self {
            threads,
            theme,
            language,
        }
    }

    fn save(self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context(message!(settings_file_has_no_parent_folder))?;
        fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut temp, &self)?;
        temp.write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|err| err.error)?;
        Ok(())
    }
}

pub(super) struct Settings {
    path: Option<PathBuf>,
    cpus: usize,
    draft: Preferences,
    saved: Preferences,
    feedback: Option<Result<()>>,
}

impl Settings {
    pub(super) fn new(ctx: &egui::Context) -> Self {
        let cpus = std::thread::available_parallelism().map_or(1, usize::from);
        let path = settings_path();
        let saved = Preferences::load(path.as_deref(), cpus);
        saved.theme.apply(ctx);
        Self {
            path,
            cpus,
            draft: saved,
            saved,
            feedback: None,
        }
    }

    pub(super) fn threads(&self) -> usize {
        self.saved.threads
    }

    pub(super) fn language(&self) -> Language {
        self.saved.language
    }

    fn save(&mut self, ctx: &egui::Context) {
        let result = self
            .path
            .as_deref()
            .context(message!(user_configuration_folder_was_not_found))
            .and_then(|path| {
                self.draft
                    .save(path)
                    .with_context(|| message!(could_not_write, path.display()))
            });
        if result.is_ok() {
            self.saved = self.draft;
            self.saved.theme.apply(ctx);
            ctx.request_repaint();
        }
        self.feedback = Some(result);
    }

    pub(super) fn ui(&mut self, ui: &mut egui::Ui) {
        let previous = self.draft;
        egui::Grid::new("preferences").show(ui, |ui| {
            ui.label(message!(worker_threads));
            ui.add(egui::DragValue::new(&mut self.draft.threads).range(1..=self.cpus));
            ui.end_row();

            ui.label(message!(appearance));
            egui::ComboBox::from_id_salt("theme")
                .selected_text(self.draft.theme.label())
                .show_ui(ui, |ui| {
                    for theme in [Theme::Light, Theme::Dark, Theme::System] {
                        ui.selectable_value(&mut self.draft.theme, theme, theme.label());
                    }
                });
            ui.end_row();
            ui.label(message!(language));
            egui::ComboBox::from_id_salt("language")
                .selected_text(self.draft.language.label())
                .show_ui(ui, |ui| {
                    for language in Language::ALL {
                        ui.selectable_value(&mut self.draft.language, language, language.label());
                    }
                });
            ui.end_row();
        });
        if self.draft != previous {
            self.feedback = None;
        }
        ui.horizontal_wrapped(|ui| {
            if ui.button(message!(save_settings)).clicked() {
                self.save(ui.ctx());
            }
            i18n::with_language(self.language(), || match &self.feedback {
                Some(Ok(())) => {
                    ui.label(message!(settings_saved));
                }
                Some(Err(err)) => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        message!(could_not_save_settings_err, err = err),
                    );
                }
                None => {}
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_defaults_and_recovers_each_invalid_field() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("settings.json");
        for (cpus, threads) in [(1, 1), (3, 1), (7, 3), (32, 8)] {
            let prefs = Preferences::load(Some(&path), cpus);
            assert_eq!(prefs.threads, threads);
            assert!(prefs.theme == Theme::System);
            assert_eq!(prefs.language, Language::Chinese);
        }
        fs::write(&path, b"{broken")?;
        assert_eq!(Preferences::load(Some(&path), 7).threads, 3);
        fs::write(
            &path,
            br#"{"threads":200,"theme":"invalid","language":"fr"}"#,
        )?;
        let prefs = Preferences::load(Some(&path), 7);
        assert_eq!(prefs.threads, 7);
        assert!(prefs.theme == Theme::System);
        assert_eq!(prefs.language, Language::Chinese);
        fs::write(&path, br#"{"threads":0,"theme":"light"}"#)?;
        let prefs = Preferences::load(Some(&path), 7);
        assert_eq!(prefs.threads, 3);
        assert!(prefs.theme == Theme::Light);
        Ok(())
    }

    #[test]
    fn saves_and_applies_preferences_only_after_persisting() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config/settings.json");
        let ctx = egui::Context::default();
        let saved = Preferences::load(None, 8);
        let mut settings = Settings {
            path: Some(path.clone()),
            cpus: 8,
            draft: Preferences {
                threads: 1,
                theme: Theme::Light,
                language: Language::English,
            },
            saved,
            feedback: None,
        };
        assert_eq!(settings.threads(), 4);
        settings.save(&ctx);
        assert!(matches!(settings.feedback, Some(Ok(()))));
        assert_eq!(settings.threads(), 1);
        assert_eq!(ctx.theme(), egui::Theme::Light);
        let loaded = Preferences::load(Some(&path), 8);
        assert_eq!(loaded.threads, 1);
        assert!(loaded.theme == Theme::Light);
        assert_eq!(loaded.language, Language::English);

        settings.path = Some(path.join("settings.json"));
        settings.draft = Preferences {
            threads: 3,
            theme: Theme::Dark,
            language: Language::Japanese,
        };
        settings.save(&ctx);
        assert!(matches!(settings.feedback, Some(Err(_))));
        assert_eq!(settings.draft.threads, 3);
        assert_eq!(settings.language(), Language::English);
        assert_eq!(settings.threads(), 1);
        assert_eq!(ctx.theme(), egui::Theme::Light);

        settings.path = Some(path.clone());
        settings.save(&ctx);
        assert_eq!(settings.threads(), 3);
        assert_eq!(ctx.theme(), egui::Theme::Dark);

        for language in Language::ALL {
            settings.draft.language = language;
            settings.save(&ctx);
            assert_eq!(settings.language(), language);
            assert_eq!(Preferences::load(Some(&path), 8).language, language);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&fs::read(&path)?)?["language"],
                language.code()
            );
        }

        settings.draft.theme = Theme::System;
        settings.save(&ctx);
        assert!(Preferences::load(Some(&path), 8).theme == Theme::System);
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let _ = ctx.run(
                egui::RawInput {
                    system_theme: Some(theme),
                    ..Default::default()
                },
                |ctx| {
                    assert_eq!(ctx.theme(), theme);
                },
            );
        }
        Ok(())
    }
}
