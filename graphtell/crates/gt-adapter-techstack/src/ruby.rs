//! Ruby tech-stack adapter.
//!
//! Ruby's conventions (Bundler manifests, Rails autoload paths, i18n layouts) are not declared here yet;
//! the parser only extracts ERB view templates, so the adapter merely needs to *serve* the language so
//! sub-project detection and the marker table agree.

use gt_domain::model::Language;
use gt_domain::port::TechStackAdapter;

pub struct RubyTechStackAdapter;

impl RubyTechStackAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl TechStackAdapter for RubyTechStackAdapter {
    fn language(&self) -> Language {
        Language::new(Language::RUBY)
    }
}
