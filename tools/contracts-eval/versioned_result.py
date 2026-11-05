"""Generate the explicit legacy/current wire adapter for a non-additive result revision."""
import json


def generate(name, definition, types):
    legacy = definition['legacy']
    current = definition['current']
    revision = definition['revision']
    variants = definition['claimVariants']
    arms = ' | '.join(f'{current}::{v} {{ claim, .. }}' for v in variants)
    copied = ', '.join(f'{field}: claim.{field}.clone()' for field in types[legacy]['fields'])
    return f'''#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("x-earlier" = {json.dumps({'name': legacy, 'before': revision})}))]
pub enum {name} {{
    #[schemars(extend("x-until" = {revision}))]
    Legacy({legacy}),
    #[schemars(extend("x-since" = {revision}))]
    Current({current}),
}}
impl Check for {name} {{
    fn check(&self) -> Result<(), String> {{
        match self {{ Self::Legacy(value) => value.check(), Self::Current(value) => value.check() }}
    }}
}}
impl Revisioned for {name} {{
    fn retain_revision(&mut self, revision: u32) {{
        if revision < {revision} {{
            let legacy = match self {{
                Self::Current({arms}) => Some({legacy} {{ {copied} }}),
                _ => None,
            }};
            if let Some(legacy) = legacy {{ *self = Self::Legacy(legacy); }}
        }}
        if let Self::Current(value) = self {{ value.retain_revision(revision); }}
    }}
    fn check_revision(&self, revision: u32) -> Result<(), String> {{
        match self {{
            Self::Legacy(_) if revision >= {revision} => Err("legacy allocation result requires its recorded revision".into()),
            Self::Current(_) if revision < {revision} => Err("allocation outcome cannot be represented at this revision".into()),
            _ => Ok(()),
        }}
    }}
}}
'''
