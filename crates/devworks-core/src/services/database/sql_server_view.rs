//! A SQL Server view is evaluated from its checked definition. The server can replace the view
//! after catalog inspection, so sending its live name would discard the admitted lineage.
use super::statement::{self, CheckedStatement, DataMode, Name, SqlRefusal};
use crate::protocol::records::DbEngine;
use sqlparser::ast::{
    Expr, Ident, ObjectName, Query, Select, SelectItem, SelectItemQualifiedWildcardKind, Statement,
    TableAlias, TableAliasColumnDef, TableFactor, VisitMut, VisitorMut,
};
use sqlparser::dialect::MsSqlDialect;
use sqlparser::parser::Parser;
use std::collections::BTreeMap;
use std::ops::ControlFlow;

#[derive(Clone)]
pub struct Definition {
    query: Box<Query>,
    columns: Vec<TableAliasColumnDef>,
    pub statement: CheckedStatement,
}

impl Definition {
    pub fn checked(sql: &str, columns: &[String]) -> Result<Self, SqlRefusal> {
        let parsed =
            Parser::parse_sql(&MsSqlDialect {}, sql).map_err(|_| SqlRefusal::InvalidStatement)?;
        let [Statement::CreateView(view)] = parsed.as_slice() else {
            return Err(SqlRefusal::InvalidStatement);
        };
        let plan = statement::check(
            &DbEngine::SqlServer,
            DataMode::Read,
            &view.query.to_string(),
        )?;
        let checked = plan
            .into_iter()
            .next()
            .ok_or(SqlRefusal::InvalidStatement)?;
        if !checked.bindings.is_empty() || columns.iter().any(|name| name.is_empty()) {
            return Err(SqlRefusal::InvalidBinding);
        }
        let columns = columns
            .iter()
            .map(|name| TableAliasColumnDef {
                name: Ident::with_quote('[', name),
                data_type: None,
            })
            .collect();
        Ok(Self {
            query: view.query.clone(),
            columns,
            statement: checked,
        })
    }
}

#[derive(Default)]
pub struct Bindings(BTreeMap<Vec<String>, Definition>);

impl Bindings {
    pub fn bind(&mut self, used: &[Name], definition: Definition) {
        self.0.insert(
            used.iter()
                .map(|part| part.value.to_ascii_lowercase())
                .collect(),
            definition,
        );
    }

    pub fn freeze(&self, plan: &CheckedStatement) -> Result<String, SqlRefusal> {
        if self.0.is_empty() {
            return Ok(plan.sql().to_owned());
        }
        // A CTE can shadow a catalog name in a nested scope. Refuse the ambiguous binding.
        for footprint in
            std::iter::once(&plan.read).chain(self.0.values().map(|view| &view.statement.read))
        {
            if footprint
                .common_tables
                .iter()
                .any(|name| self.0.contains_key(&vec![name.value.to_ascii_lowercase()]))
            {
                return Err(SqlRefusal::UnqualifiedRelation);
            }
        }
        let mut parsed = Parser::parse_sql(&MsSqlDialect {}, plan.sql())
            .map_err(|_| SqlRefusal::InvalidStatement)?;
        let [statement @ Statement::Query(_)] = parsed.as_mut_slice() else {
            return Err(SqlRefusal::InvalidStatement);
        };
        match VisitMut::visit(
            statement,
            &mut Inline {
                views: self,
                expanded: 0,
            },
        ) {
            ControlFlow::Break(reason) => Err(reason),
            ControlFlow::Continue(()) => {
                let sql = statement.to_string();
                if sql.len() > 256 * 1024 {
                    return Err(SqlRefusal::UnqualifiedRelation);
                }
                Ok(sql)
            }
        }
    }
}

struct Inline<'a> {
    views: &'a Bindings,
    expanded: usize,
}

fn object_parts(name: &ObjectName) -> Option<Vec<String>> {
    name.0
        .iter()
        .map(|name| name.as_ident().map(|name| name.value.to_ascii_lowercase()))
        .collect()
}

impl VisitorMut for Inline<'_> {
    type Break = SqlRefusal;

    fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<Self::Break> {
        let TableFactor::Table { name, alias, .. } = factor else {
            return ControlFlow::Continue(());
        };
        let Some(key) = object_parts(name) else {
            return ControlFlow::Break(SqlRefusal::UnqualifiedRelation);
        };
        let Some(view) = self.views.0.get(&key) else {
            return ControlFlow::Continue(());
        };
        self.expanded += 1;
        if self.expanded > 128 {
            return ControlFlow::Break(SqlRefusal::UnqualifiedRelation);
        }
        let Some(last) = name.0.last().and_then(|part| part.as_ident()).cloned() else {
            return ControlFlow::Break(SqlRefusal::UnqualifiedRelation);
        };
        let mut alias = alias.clone().unwrap_or(TableAlias {
            explicit: true,
            name: last,
            columns: Vec::new(),
            at: None,
        });
        if alias.columns.is_empty() {
            alias.columns.clone_from(&view.columns);
        }
        *factor = TableFactor::Derived {
            lateral: false,
            subquery: view.query.clone(),
            alias: Some(alias),
            sample: None,
        };
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<Self::Break> {
        if let Expr::CompoundIdentifier(path) = expression
            && path.len() >= 3
            && self.views.0.contains_key(
                &path[..path.len() - 1]
                    .iter()
                    .map(|name| name.value.to_ascii_lowercase())
                    .collect::<Vec<_>>(),
            )
        {
            path.drain(..path.len() - 2);
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_select(&mut self, select: &mut Select) -> ControlFlow<Self::Break> {
        for item in &mut select.projection {
            if let SelectItem::QualifiedWildcard(
                SelectItemQualifiedWildcardKind::ObjectName(name),
                _,
            ) = item
                && name.0.len() > 1
                && object_parts(name).is_some_and(|parts| self.views.0.contains_key(&parts))
            {
                name.0.drain(..name.0.len() - 1);
            }
        }
        ControlFlow::Continue(())
    }
}
