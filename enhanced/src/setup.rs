//! The first-time setup window ([`gw2enhanced_core::setup`]): a welcome page (the overlay language), then the pages of
//! each feature built in that still needs them. A build with one feature shows that feature's setup; a build with
//! several shows one setup for all, with one "run the setup again" button (the Version tab).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use hudhook::imgui::{Condition, Ui, WindowFlags};

use gw2enhanced_core::i18n::tr;
use gw2enhanced_core::setup;

use crate::{Overlay, full_build, language_picker};

/// The setup waits for the game to finish its first loading screen.
pub const SETUP_DELAY: Duration = Duration::from_secs(20);

/// The setup window is on screen (the host draws the cursor and takes the input).
pub static SHOWN: AtomicBool = AtomicBool::new(false);

/// The features with setup pages, in the order they are shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    Welcome,
    #[cfg(feature = "translation")]
    Translation,
    #[cfg(feature = "textures")]
    Textures,
    #[cfg(feature = "graphics")]
    Graphics,
}

impl Part {
    fn title(self) -> &'static str {
        match self {
            Part::Welcome => tr("Welcome", "Boas-vindas"),
            #[cfg(feature = "translation")]
            Part::Translation => tr("Translation", "Tradução"),
            #[cfg(feature = "textures")]
            Part::Textures => tr("HD textures", "Texturas HD"),
            #[cfg(feature = "graphics")]
            Part::Graphics => tr("Graphics", "Gráficos"),
        }
    }
}

/// The name of this build's setup.
fn window_title() -> &'static str {
    if full_build() {
        return tr("GW2 Enhanced setup", "Configuração do GW2 Enhanced");
    }
    #[cfg(feature = "translation")]
    return tr("GW2 Translated setup", "Configuração do GW2 Translated");
    #[cfg(all(not(feature = "translation"), feature = "textures"))]
    return tr("GW2 Remastered setup", "Configuração do GW2 Remastered");
    #[cfg(all(not(feature = "translation"), not(feature = "textures"), feature = "graphics"))]
    return tr("GW2 Graphics setup", "Configuração do GW2 Graphics");
    #[allow(unreachable_code)]
    tr("Setup", "Configuração")
}

impl Overlay {
    /// The parts with pages now: every feature built in when the setup was asked for again, else the ones still pending.
    fn setup_parts(&self) -> Vec<(Part, usize)> {
        let all = self.setup_again;
        let mut v = Vec::new();
        #[cfg(feature = "translation")]
        if self.translated.is_some() && (all || gw2translated::overlay::Overlay::setup_pending()) {
            v.push((Part::Translation, gw2translated::overlay::Overlay::SETUP_PAGES));
        }
        #[cfg(feature = "textures")]
        if self.textures.is_some() && (all || gw2remastered::overlay::Overlay::setup_pending()) {
            v.push((Part::Textures, gw2remastered::overlay::Overlay::SETUP_PAGES));
        }
        #[cfg(feature = "graphics")]
        if self.graphics.is_some() && (all || gw2graphics::overlay::Overlay::setup_pending()) {
            v.push((Part::Graphics, gw2graphics::overlay::Overlay::SETUP_PAGES));
        }
        if !v.is_empty() {
            v.insert(0, (Part::Welcome, 1));
        }
        v
    }

    /// Ends the setup: every feature shown is marked done (finished or skipped alike).
    fn setup_finish(&mut self, parts: &[(Part, usize)]) {
        for (p, _) in parts {
            match p {
                Part::Welcome => {}
                #[cfg(feature = "translation")]
                Part::Translation => gw2translated::overlay::Overlay::setup_finish(),
                #[cfg(feature = "textures")]
                Part::Textures => gw2remastered::overlay::Overlay::setup_finish(),
                #[cfg(feature = "graphics")]
                Part::Graphics => gw2graphics::overlay::Overlay::setup_finish(),
            }
        }
        self.setup_again = false;
        self.setup_page = 0;
        self.setup_list = None;
    }

    /// Draws the setup window when it is due (once per frame).
    pub(crate) fn setup_window(&mut self, ui: &Ui) {
        if setup::take_again() {
            self.setup_again = true;
            self.setup_page = 0;
            self.setup_list = None;
            crate::host_hide();
        }
        // The pages are fixed when the setup opens (a page that completes its feature, like choosing the pack folder,
        // must not make the pages after it move).
        if self.setup_list.is_none() && (self.started.elapsed() >= SETUP_DELAY || self.setup_again) {
            let p = self.setup_parts();
            if !p.is_empty() {
                self.setup_list = Some(p);
            }
        }
        let parts = self.setup_list.clone().unwrap_or_default();
        let counts: Vec<usize> = parts.iter().map(|(_, n)| *n).collect();
        let total: usize = counts.iter().sum();
        let Some((pi, page)) = setup::locate(&counts, self.setup_page.min(total.saturating_sub(1))).filter(|_| total > 0) else {
            SHOWN.store(false, Ordering::Relaxed);
            return;
        };
        SHOWN.store(true, Ordering::Relaxed);
        let part = parts[pi].0;
        let step = self.setup_page.min(total - 1);
        let [w, h] = ui.io().display_size;
        let title = format!("{}  ({}/{})###gw2enhanced-setup", window_title(), step + 1, total);
        let mut next = false;
        let mut finish = false;
        ui.window(title)
            .position([w / 2.0, h / 2.0], Condition::Always)
            .position_pivot([0.5, 0.5])
            .size([620.0 * ui.io().font_global_scale, 0.0], Condition::Always)
            .flags(WindowFlags::NO_COLLAPSE | WindowFlags::NO_RESIZE | WindowFlags::NO_MOVE)
            .build(|| {
                ui.text_colored([0.55, 0.8, 1.0, 1.0], part.title());
                ui.separator();
                let answered = match part {
                    Part::Welcome => {
                        ui.text_wrapped(if full_build() {
                            tr(
                                "Let's set up the mod in a few steps, one part per feature. You can change everything later in the overlay.",
                                "Vamos configurar o mod em poucos passos, uma parte por recurso. Você pode mudar tudo depois no overlay.",
                            )
                        } else {
                            tr(
                                "Let's set up the mod in a few steps. You can change everything later in the overlay.",
                                "Vamos configurar o mod em poucos passos. Você pode mudar tudo depois no overlay.",
                            )
                        });
                        ui.spacing();
                        language_picker(ui);
                        false
                    }
                    #[cfg(feature = "translation")]
                    Part::Translation => self.translated.as_mut().is_some_and(|o| o.setup_page(ui, page)),
                    #[cfg(feature = "textures")]
                    Part::Textures => self.textures.as_mut().is_some_and(|o| o.setup_page(ui, page)),
                    #[cfg(feature = "graphics")]
                    Part::Graphics => self.graphics.as_mut().is_some_and(|o| o.setup_page(ui, page)),
                };
                ui.spacing();
                ui.separator();
                if step > 0 && ui.button(tr("Back###setup-back", "Voltar###setup-back")) {
                    self.setup_page = step - 1;
                }
                let last = step + 1 >= total;
                if step > 0 {
                    ui.same_line();
                }
                if ui.button(if last { tr("Finish###setup-next", "Concluir###setup-next") } else { tr("Next###setup-next", "Próximo###setup-next") }) || answered {
                    next = true;
                }
                if !last {
                    ui.same_line();
                    if ui.button(tr("Skip the setup###setup-skip", "Pular a configuração###setup-skip")) {
                        finish = true;
                    }
                }
                let _ = page;
            });
        if next {
            if step + 1 >= total {
                finish = true;
            } else {
                self.setup_page = step + 1;
            }
        }
        if finish {
            self.setup_finish(&parts);
            SHOWN.store(false, Ordering::Relaxed);
        }
    }
}
