//! Text shown to the player in the overlay, per language.
//!
//! It follows the overlay language (`gw2enhanced_core::i18n`), not the language
//! the game is translated into; code, logs and docs stay in English.

use gw2enhanced_core::i18n::Lang;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum T {
    Title,
    Language,
    LanguageRestart,
    Options,
    Translate,
    LiveTranslate,
    WebTranslate,
    Collect,
    Contribute,
    ContributeHint,
    RefreshNow,
    Status,
    StatusTexts,
    StatusLive,
    StatusLiveOff,
    CloseHint,
    PopupTitle,
    PopupBody,
    PopupYes,
    PopupNo,
    Resources,
    WizTitle,
    WizStep,
    WizIntro,
    WizTranslateTitle,
    WizDeviceTitle,
    WizDeviceHint,
    DeviceAuto,
    DeviceCuda,
    DeviceDirectMl,
    DeviceCpu,
    GpuDetected,
    GpuNone,
    GpuFree,
    GpuNotEnough,
    DeviceInUse,
    GpuComing,
    TranslatorOff,
    Threads,
    WizBubbleTitle,
    BubbleWait,
    BubbleWaitHint,
    Performance,
    Back,
    Next,
    Skip,
    OnboardAgain,
    UpdateDictionary,
    DictionaryVersion,
    DictionaryShipped,
}

fn en(t: T) -> &'static str {
    match t {
        T::Title => "GW2 Translated",
        T::Language => "Game translation language",
        T::LanguageRestart => "The new translation language is used after restarting the game.",
        T::Options => "Options",
        T::Translate => "Translate the game",
        T::LiveTranslate => "Translate missing texts on this PC",
        T::WebTranslate => "Translate the Trading Post and Gem Store pages",
        T::Collect => "Collect untranslated texts",
        T::Contribute => "Help improve the translation",
        T::ContributeHint => {
            "Sends English game texts and the translations made on this PC. Never chat or personal data."
        }
        T::RefreshNow => "Refresh texts now",
        T::Status => "Status",
        T::StatusTexts => "texts decoded / translated",
        T::StatusLive => "local translation: on screen / ahead / queued",
        T::StatusLiveOff => "local translation is off",
        T::CloseHint => "Press the overlay key again to close.",
        T::PopupTitle => "Help improve GW2 Translated?",
        T::PopupBody => {
            "GW2 Translated can send the English game texts it finds, and the translations made on \
            this PC, to our server, so they get reviewed and improved for everyone. Only texts that come from the \
            game files are sent: never chat, character names or anything personal. You can change this later \
            in the overlay."
        }
        T::PopupYes => "Yes, I want to help",
        T::PopupNo => "Not now",
        T::Resources => "Translator usage:",
        T::WizTitle => "Welcome to GW2 Translated",
        T::WizStep => "Step",
        T::WizIntro => {
            "Let's set up the translation in a few steps. You can change everything later in this overlay (Insert key)."
        }
        T::WizTranslateTitle => "What to translate",
        T::WizDeviceTitle => "Local translator",
        T::WizDeviceHint => {
            "Translates on this PC whatever the dictionary doesn't have yet. A graphics card is much faster; the processor works on any PC."
        }
        T::DeviceAuto => "Automatic (recommended)",
        T::DeviceCuda => "NVIDIA graphics card (CUDA)",
        T::DeviceDirectMl => "Any graphics card (DirectML: NVIDIA, AMD, Intel)",
        T::DeviceCpu => "Processor (CPU)",
        T::GpuDetected => "Graphics cards found:",
        T::GpuNone => "No graphics card with enough free memory: the processor will be used.",
        T::GpuFree => "MB free",
        T::GpuNotEnough => "(not enough free memory)",
        T::DeviceInUse => "In use now:",
        T::TranslatorOff => "the local translator is not running (it starts when the game is in English).",
        T::GpuComing => {
            "The graphics card could not be used (missing GPU pack, driver or free memory): the processor is translating."
        }
        T::Threads => "Processor threads (0 = automatic)",
        T::WizBubbleTitle => "Speech bubbles",
        T::BubbleWait => "Extra time while translating (seconds, at most)",
        T::BubbleWaitHint => "Longer lines wait longer, up to this limit. 0 turns it off.",
        T::Performance => "Performance",
        T::Back => "Back",
        T::Next => "Next",
        T::Skip => "Skip setup",
        T::OnboardAgain => "Run the first-time setup again",
        T::UpdateDictionary => "Update the dictionary automatically",
        T::DictionaryVersion => "Dictionary: downloaded version",
        T::DictionaryShipped => "Dictionary: the one installed with the mod",
    }
}

fn pt_br(t: T) -> &'static str {
    match t {
        T::Title => "GW2 Translated",
        T::Language => "Idioma da tradução do jogo",
        T::LanguageRestart => "O novo idioma da tradução vale depois de reiniciar o jogo.",
        T::Options => "Opções",
        T::Translate => "Traduzir o jogo",
        T::LiveTranslate => "Traduzir no PC o que faltar",
        T::WebTranslate => "Traduzir as páginas da Companhia Comercial e da Loja de Gemas",
        T::Collect => "Coletar textos sem tradução",
        T::Contribute => "Ajudar a melhorar a tradução",
        T::ContributeHint => {
            "Envia textos do jogo em inglês e as traduções feitas no seu PC. Nunca o chat ou dados pessoais."
        }
        T::RefreshNow => "Atualizar textos agora",
        T::Status => "Situação",
        T::StatusTexts => "textos decodificados / traduzidos",
        T::StatusLive => "tradução local: na tela / antecipados / na fila",
        T::StatusLiveOff => "tradução local desligada",
        T::CloseHint => "Aperte a tecla do overlay de novo para fechar.",
        T::PopupTitle => "Quer ajudar a melhorar o GW2 Translated?",
        T::PopupBody => {
            "O GW2 Translated pode enviar para o nosso servidor os textos do jogo em inglês que \
            encontrar e as traduções feitas no seu PC, para serem revisados e melhorados para todo mundo. Só vão \
            textos que vêm dos arquivos do jogo: nunca o chat, nomes de personagens ou qualquer dado pessoal. \
            Dá para mudar isso depois no overlay."
        }
        T::PopupYes => "Sim, quero ajudar",
        T::PopupNo => "Agora não",
        T::Resources => "Uso do tradutor:",
        T::WizTitle => "Bem-vindo ao GW2 Translated",
        T::WizStep => "Passo",
        T::WizIntro => {
            "Vamos configurar a tradução em poucos passos. Dá para mudar tudo depois neste overlay (tecla Insert)."
        }
        T::WizTranslateTitle => "O que traduzir",
        T::WizDeviceTitle => "Tradutor local",
        T::WizDeviceHint => {
            "Traduz no seu PC o que o dicionário ainda não tem. A placa de vídeo é bem mais rápida; o processador funciona em qualquer PC."
        }
        T::DeviceAuto => "Automático (recomendado)",
        T::DeviceCuda => "Placa de vídeo NVIDIA (CUDA)",
        T::DeviceDirectMl => "Qualquer placa de vídeo (DirectML: NVIDIA, AMD, Intel)",
        T::DeviceCpu => "Processador (CPU)",
        T::GpuDetected => "Placas de vídeo encontradas:",
        T::GpuNone => "Nenhuma placa de vídeo com memória livre suficiente: o processador será usado.",
        T::GpuFree => "MB livres",
        T::GpuNotEnough => "(memória livre insuficiente)",
        T::DeviceInUse => "Em uso agora:",
        T::TranslatorOff => "o tradutor local não está rodando (ele inicia quando o jogo está em inglês).",
        T::GpuComing => {
            "Não foi possível usar a placa de vídeo (falta o pacote de GPU, driver ou memória livre): o processador está traduzindo."
        }
        T::Threads => "Threads do processador (0 = automático)",
        T::WizBubbleTitle => "Balões de fala",
        T::BubbleWait => "Tempo extra enquanto traduz (segundos, no máximo)",
        T::BubbleWaitHint => "Falas maiores esperam mais, até esse limite. 0 desliga.",
        T::Performance => "Desempenho",
        T::Back => "Voltar",
        T::Next => "Avançar",
        T::Skip => "Pular configuração",
        T::OnboardAgain => "Refazer a configuração inicial",
        T::UpdateDictionary => "Atualizar dicionário automaticamente",
        T::DictionaryVersion => "Dicionário: versão baixada",
        T::DictionaryShipped => "Dicionário: o que veio com o mod",
    }
}

/// Text for `t` in the overlay language `lang`.
pub fn text(lang: Lang, t: T) -> &'static str {
    match lang {
        Lang::En => en(t),
        Lang::PtBr => pt_br(t),
    }
}

/// Name of a language as shown in the selector.
pub fn language_name(code: &str) -> String {
    match code {
        "pt-BR" => "Português (Brasil)".into(),
        "es" => "Español".into(),
        "fr" => "Français".into(),
        "de" => "Deutsch".into(),
        _ => code.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key; `en` and `pt_br` are exhaustive matches, so a key without a text does not compile.
    const ALL: &[T] = &[
        T::Title, T::Language, T::LanguageRestart, T::Options, T::Translate, T::LiveTranslate, T::WebTranslate,
        T::Collect, T::Contribute, T::ContributeHint, T::RefreshNow, T::Status, T::StatusTexts, T::StatusLive,
        T::StatusLiveOff, T::CloseHint, T::PopupTitle, T::PopupBody, T::PopupYes, T::PopupNo, T::Resources,
        T::WizTitle, T::WizStep, T::WizIntro, T::WizTranslateTitle, T::WizDeviceTitle, T::WizDeviceHint,
        T::DeviceAuto, T::DeviceCuda, T::DeviceDirectMl, T::DeviceCpu, T::GpuDetected, T::GpuNone, T::GpuFree,
        T::GpuNotEnough, T::DeviceInUse, T::GpuComing, T::TranslatorOff, T::Threads, T::WizBubbleTitle,
        T::BubbleWait, T::BubbleWaitHint, T::Performance, T::Back, T::Next, T::Skip, T::OnboardAgain,
    ];

    #[test]
    fn every_key_has_both_languages() {
        for &t in ALL {
            let (en, pt) = (text(Lang::En, t), text(Lang::PtBr, t));
            assert!(!en.trim().is_empty() && !pt.trim().is_empty(), "{t:?}");
            // Only the product name is the same in both.
            assert!(en != pt || t == T::Title, "{t:?} is not translated");
        }
    }

    #[test]
    fn the_translation_language_is_labelled_as_such() {
        assert!(text(Lang::En, T::Language).contains("translation"));
        assert!(text(Lang::PtBr, T::Language).contains("tradução"));
    }
}
