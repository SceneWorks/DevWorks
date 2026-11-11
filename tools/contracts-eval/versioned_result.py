"""Generate the explicit legacy/current wire adapter for a non-additive result revision."""
import json
import re


def generate(name, definition, types):
    legacy = definition['legacy']
    current = definition['current']
    revision = definition['revision']
    variants = definition.get('claimVariants')
    if variants is None and definition.get('projection') not in ('none', 'help_read', 'help_page', 'convention_record', 'convention_read', 'convention_page', 'convention_resources', 'convention_search', 'convention_mutation', 'page_field'):
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
    if definition.get('projection', '').startswith('convention_'):
        conversion = convention_conversion(definition)
    if definition.get('projection') == 'page_field':
        conversion = page_field_conversion(definition)
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
    # An empty page is the same JSON in both shapes (the untagged reader takes it as legacy), so
    # a page_field result admits an empty legacy page at every revision.
    legacy_arm = 'Self::Legacy(value) if !value.items.0.is_empty() &&' if definition.get('projection') == 'page_field' else 'Self::Legacy(_) if'
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
            {legacy_arm} revision >= {revision} => Err("{legacy_error}".into()),
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


def convention_conversion(definition):
    """Project real recorded identities/content; lifecycle never supplies approval."""
    legacy, current, revision = (definition[k] for k in ('legacy', 'current', 'revision'))
    kind = definition['projection']
    project = """            let project = |topic: &ConventionRecord| -> Option<InstructionRecord> {
                if !(1..=256).contains(&topic.title.0.chars().count()) || topic.title.0.contains('\\0') { return None; }
                let content = topic.content.clone()?;
                let row = InstructionRecord { id: topic.id.clone(), revision: topic.revision.clone(),
                    name: BoundedText(topic.title.0.clone()), scope: topic.scope.clone(), content,
                    source_path: None, approved: topic.legacy_approved, applicability: topic.applicability.clone() };
                row.check().ok().map(|()| row)
            };\n"""
    resource = """            let resource = |topic: &ConventionResourceRecord| -> Option<InstructionRecord> {
                if !(1..=256).contains(&topic.name.0.chars().count()) || topic.name.0.contains('\\0') { return None; }
                let row = InstructionRecord { id: topic.id.clone(), revision: topic.revision.clone(),
                    name: BoundedText(topic.name.0.clone()), scope: topic.scope.clone(), content: topic.content.clone(),
                    source_path: None, approved: false, applicability: topic.applicability.clone() };
                row.check().ok().map(|()| row)
            };\n"""
    if kind == 'convention_mutation':
        helpers = ''
        arm = 'Self::Current(value) => Some(MutationSummary { id: value.id.clone(), revision: value.revision.clone(), changed: value.changed }),'
    elif kind == 'convention_record':
        helpers = project
        arm = 'Self::Current(value) => project(value),'
    elif kind == 'convention_read':
        helpers = project
        arm = 'Self::Current(ConventionReadOutcome::Document { view }) if matches!(view.format, ConventionFormat::Markdown) => project(&view.convention),'
    elif kind == 'convention_resources':
        helpers = project + resource
        arm = """Self::Current(ConventionResourcesView::Catalog { page }) => page.items.0.iter().map(project).collect::<Option<Vec<_>>>()
                    .map(|items| Page { items: BoundedList(items), next_cursor: page.next_cursor.clone(), has_more: page.has_more }),
                Self::Current(ConventionResourcesView::Attachments { page }) => page.items.0.iter().map(resource).collect::<Option<Vec<_>>>()
                    .map(|items| Page { items: BoundedList(items), next_cursor: page.next_cursor.clone(), has_more: page.has_more }),"""
    else:
        helpers = project
        mapping = '|topic| project(&topic.convention)' if kind == 'convention_search' else 'project'
        arm = f'''Self::Current(value) => value.items.0.iter().map({mapping}).collect::<Option<Vec<_>>>()
                    .map(|items| Page {{ items: BoundedList(items), next_cursor: value.next_cursor.clone(), has_more: value.has_more }}),'''
    return f'''        if revision < {revision} {{
{helpers}            let legacy: Option<{legacy}> = match self {{
                {arm}
                _ => None,
            }};
            if let Some(legacy) = legacy {{ *self = Self::Legacy(legacy); }}
        }}
'''


def page_field_conversion(definition):
    """Each current page item carries its legacy item as one field; keep that field, order and cursor."""
    legacy, revision, field = (definition[k] for k in ('legacy', 'revision', 'itemField'))
    return f'''        if revision < {revision} {{
            let legacy: Option<{legacy}> = match self {{
                Self::Current(page) => Some(Page {{ items: BoundedList(page.items.0.iter().map(|item| item.{field}.clone()).collect()),
                    next_cursor: page.next_cursor.clone(), has_more: page.has_more }}),
                _ => None,
            }};
            if let Some(legacy) = legacy {{ *self = Self::Legacy(legacy); }}
        }}
'''
