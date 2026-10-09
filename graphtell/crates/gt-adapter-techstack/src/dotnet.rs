//! .NET tech-stack adapter.
//!
//! .NET's conventions (NuGet manifests, MSBuild properties, i18n resource layouts) are not declared here
//! yet; the parser only extracts Razor view templates, so the adapter merely needs to *serve* the language
//! so sub-project detection and the marker table agree.

use gt_domain::model::Language;
use gt_domain::port::TechStackAdapter;

pub struct DotnetTechStackAdapter;

impl DotnetTechStackAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl TechStackAdapter for DotnetTechStackAdapter {
    fn language(&self) -> Language {
        Language::new(Language::CSHARP)
    }
}
