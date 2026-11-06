"""Generate the explicit legacy/current wire adapter for a non-additive result revision."""
import json
import re


def generate(name, definition, types):
    legacy = definition['legacy']
    current = definition['current']
    revision = definition['revision']
    variants = definition.get('claimVariants')
    if variants is None and definition.get('projection') not in ('none', 'help_read', 'help_page'):
        raise ValueError(f'{name}: an explicit legacy projection strategy is required')
    conversion = ''
    if variants:
        arms = ' | '.join(f'{current}::{v} {{ claim, .. }}' for v in variants)
        copied = ', '.join(f'{field}: claim.{field}.clone()' for field in types[legacy]['fields'])
        conversion = f"""        if revision < {revision} {{
            let legacy = match self {{
                Self::Current({arms}) => Some({legacy} {{ {copied} }}),
                _ => None,
            }};
            if let Some(legacy) = legacy {{ *self = Self::Legacy(legacy); }}
        }}
"""
    if definition.get('projection', '').startswith('help_'):
        conversion = help_conversion(definition, types)
    def revisioned(ty, seen=None):
        seen = set() if seen is None else seen
        for token in re.findall(r'[A-Za-z_][A-Za-z0-9_]*', ty):
            if token in seen or token not in types:
                continue
            seen.add(token)
            d = types[token]
            if d.get('since') or d.get('variantSince') or d['kind'] == 'versioned':
                return True
            fields = d.get('fields', {}).values() if d['kind'] == 'record' else [v for fs in d.get('variants', {}).values() for v in fs.values()]
            if any(revisioned(v, seen) for v in fields):
                return True
        return False
    retain = '        if let Self::Current(value) = self { value.retain_revision(revision); }' if revisioned(current) else ''
    parameter = 'revision' if conversion or retain else '_revision'
    earlier = definition.get('earlierName', legacy)
    legacy_error = 'legacy allocation result requires its recorded revision' if variants else 'legacy result requires its recorded revision'
    current_error = 'allocation outcome cannot be represented at this revision' if variants else 'current result cannot be represented at this revision'
    return f'''#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("x-earlier" = {json.dumps({'name': earlier, 'before': revision})}))]
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
    fn retain_revision(&mut self, {parameter}: u32) {{
{conversion}{retain}
    }}
    fn check_revision(&self, revision: u32) -> Result<(), String> {{
        match self {{
            Self::Legacy(_) if revision >= {revision} => Err("{legacy_error}".into()),
            Self::Current(_) if revision < {revision} => Err("{current_error}".into()),
            _ => Ok(()),
        }}
    }}
}}
'''


def help_conversion(definition, types):
    """Basic Help projections retain actual published artifact references, never fabricated IDs."""
    legacy, current, revision = (definition[k] for k in ('legacy', 'current', 'revision'))
    fields = ', '.join(f'{field}: topic.{field}.clone()' for field in types['HelpTopic']['fields'] if field != 'content')
    if definition['projection'] == 'help_read':
        projection = f'''Self::Current({current}::Available {{ topic, .. }}) => topic.content.clone().map(|content| HelpTopic {{ {fields}, content }}),'''
    else:
        projection = f'''Self::Current(value) if matches!(value.availability, HelpAvailability::Available)
                && value.items.0.iter().all(|topic| matches!(topic.availability, HelpAvailability::Available) && topic.content.is_some()) =>
                    Some(Page {{ items: BoundedList(value.items.0.iter().map(|topic| HelpTopic {{ {fields}, content: topic.content.clone().expect("checked published artifact") }}).collect()),
                        next_cursor: value.next_cursor.clone(), has_more: value.has_more }}),'''
    return f'''        if revision < {revision} {{
            let legacy: Option<{legacy}> = match self {{
                {projection}
                _ => None,
            }};
            if let Some(legacy) = legacy {{ *self = Self::Legacy(legacy); }}
        }}
'''
