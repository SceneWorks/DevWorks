//! Classify and inspect typed SQL against the selected target dialect before contacting it.
//! The canonical SQL sent to a driver is derived from the parsed tree, never raw comments.
use crate::protocol::records::DbEngine;
use sqlparser::ast::{
    Expr, Ident, ObjectName, ObjectType, Query, Select, SelectItem,
    SelectItemQualifiedWildcardKind, Statement, TableFactor, TableObject, Value, ValueWithSpan,
    Visit, VisitMut, Visitor, VisitorMut,
};
use sqlparser::dialect::{Dialect, MsSqlDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect};
use sqlparser::parser::Parser;
use std::collections::BTreeSet;
use std::ops::ControlFlow;

const MAX_SQL_BYTES: usize = 64 * 1024;
const MAX_MIGRATION_STATEMENTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataMode {
    Read,
    Write,
    Schema,
    Migration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqlRefusal {
    InvalidStatement,
    StatementCount,
    WrongMode,
    UnqualifiedExpression,
    UnqualifiedRelation,
    InvalidBinding,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Name {
    pub value: String,
    pub quoted: bool,
}

impl From<&Ident> for Name {
    fn from(value: &Ident) -> Self {
        Self {
            value: value.value.clone(),
            quoted: value.quote_style.is_some(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Relation {
    pub path: Vec<Name>,
    pub alias: Option<Name>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColumnAccess {
    Named(Vec<Name>),
    Wildcard(Vec<Name>),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Footprint {
    pub relations: BTreeSet<Relation>,
    pub columns: BTreeSet<ColumnAccess>,
    pub common_tables: BTreeSet<Name>,
    pub derived_aliases: BTreeSet<Name>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inspection {
    TableListing {
        catalog: Option<Vec<Name>>,
        views_only: bool,
    },
    CatalogListing,
    RoutineListing,
    Variables(Option<String>),
    TableDefinition,
    QueryPlan,
}

#[derive(Clone)]
pub struct CheckedStatement {
    sql: String,
    pub read: Footprint,
    pub bindings: Vec<String>,
    pub schema_change: bool,
    pub renames_schema_identity: bool,
    pub created_relation: Option<Vec<Name>>,
    pub inspection: Option<Inspection>,
}

impl CheckedStatement {
    pub fn sql(&self) -> &str {
        &self.sql
    }
}

fn dialect(engine: &DbEngine) -> &'static dyn Dialect {
    match engine {
        DbEngine::Sqlite => &SQLiteDialect {},
        DbEngine::MariaDb | DbEngine::MySql => &MySqlDialect {},
        DbEngine::Postgres => &PostgreSqlDialect {},
        DbEngine::SqlServer => &MsSqlDialect {},
    }
}

fn simple_name(name: &ObjectName) -> Result<Vec<Name>, SqlRefusal> {
    if name.0.is_empty() || name.0.len() > 3 {
        return Err(SqlRefusal::UnqualifiedRelation);
    }
    name.0
        .iter()
        .map(|part| {
            part.as_ident()
                .map(Name::from)
                .filter(|name| !name.value.is_empty() && !name.value.chars().any(char::is_control))
                .ok_or(SqlRefusal::UnqualifiedRelation)
        })
        .collect()
}

fn write_statement(statement: &Statement) -> bool {
    match statement {
        Statement::Insert(insert) => {
            matches!(insert.table, TableObject::TableName(_))
                && insert.returning.is_none()
                && insert.output.is_none()
                && !insert.replace_into
                && !insert.overwrite
                && insert.optimizer_hints.is_empty()
                && insert.settings.is_none()
                && insert.format_clause.is_none()
        }
        Statement::Update(update) => {
            update.returning.is_none()
                && update.output.is_none()
                && update.optimizer_hints.is_empty()
                && update.settings.is_none()
                && update.on_cluster.is_none()
        }
        Statement::Delete(delete) => {
            delete.returning.is_none()
                && delete.output.is_none()
                && delete.optimizer_hints.is_empty()
                && delete.settings.is_none()
                && delete.on_cluster.is_none()
        }
        _ => false,
    }
}

fn schema_statement(statement: &Statement, engine: &DbEngine) -> bool {
    use sqlparser::ast::{
        AlterTableOperation as Alter, CreateTableOptions, RenameTableNameKind, SqlOption,
    };

    match statement {
        Statement::CreateTable(table) => {
            let options_allowed = match &table.table_options {
                CreateTableOptions::None => true,
                CreateTableOptions::Plain(options)
                    if matches!(engine, DbEngine::MySql | DbEngine::MariaDb) =>
                {
                    options.iter().all(|option| match option {
                        SqlOption::Comment(_) => true,
                        SqlOption::KeyValue { key, value } => {
                            key.to_string().eq_ignore_ascii_case("engine")
                                && matches!(value, Expr::Identifier(name) if name.value.eq_ignore_ascii_case("innodb"))
                        }
                        _ => false,
                    })
                }
                _ => false,
            };
            options_allowed
                && (matches!(engine, DbEngine::Sqlite)
                    || (!table.if_not_exists && !table.or_replace))
                && !table.temporary
                && !table.external
                && table.location.is_none()
                && table.file_format.is_none()
                && table.on_cluster.is_none()
                && table.inherits.is_none()
                && table.partition_of.is_none()
                && table.clone.is_none()
                && table.like.is_none()
        }
        Statement::CreateView(view) => {
            !view.temporary
                && (matches!(engine, DbEngine::Sqlite)
                    || (!view.if_not_exists && !view.or_replace && !view.or_alter))
                && !view.materialized
                && !view.with_no_schema_binding
                && view.params.is_none()
                && view.to.is_none()
                && matches!(view.options, CreateTableOptions::None)
        }
        Statement::AlterTable(table) => {
            table.location.is_none()
                && table.on_cluster.is_none()
                && table.table_type.is_none()
                && table.operations.iter().all(|change| match change {
                    Alter::AddConstraint { .. }
                    | Alter::AddColumn { .. }
                    | Alter::DropConstraint { .. }
                    | Alter::DropColumn { .. }
                    | Alter::DropPrimaryKey { .. }
                    | Alter::DropForeignKey { .. }
                    | Alter::DropIndex { .. }
                    | Alter::RenameColumn { .. }
                    | Alter::RenameConstraint { .. }
                    | Alter::ChangeColumn { .. }
                    | Alter::ModifyColumn { .. }
                    | Alter::AlterColumn { .. } => true,
                    // A rename may not move a table into a different schema.
                    Alter::RenameTable {
                        table_name: RenameTableNameKind::As(table_name)
                            | RenameTableNameKind::To(table_name),
                    } => {
                        simple_name(table_name).is_ok_and(|name| name.len() == 1)
                    }
                    _ => false,
                })
        }
        Statement::CreateIndex(index) => !index.concurrently,
        Statement::Drop {
            object_type: ObjectType::Table | ObjectType::View | ObjectType::Index,
            cascade: false,
            temporary: false,
            ..
        } => true,
        _ => false,
    }
}

fn rename_destination(table: &sqlparser::ast::AlterTable) -> Result<Option<Vec<Name>>, SqlRefusal> {
    use sqlparser::ast::{AlterTableOperation, RenameTableNameKind};
    let mut destination = None;
    for operation in &table.operations {
        if let AlterTableOperation::RenameTable {
            table_name: RenameTableNameKind::As(name) | RenameTableNameKind::To(name),
        } = operation
        {
            let new = simple_name(name)?;
            if new.len() != 1 || destination.is_some() || table.operations.len() != 1 {
                return Err(SqlRefusal::UnqualifiedRelation);
            }
            let mut path = simple_name(&table.name)?;
            path.pop();
            path.extend(new);
            destination = Some(path);
        }
    }
    Ok(destination)
}

fn read_statement(statement: &Statement, engine: &DbEngine) -> bool {
    match statement {
        Statement::Query(_) => true,
        Statement::Explain {
            analyze: false,
            estimate: false,
            statement,
            options: None,
            ..
        } => matches!(statement.as_ref(), Statement::Query(_)),
        Statement::ExplainTable { .. }
        | Statement::ShowTables { .. }
        | Statement::ShowViews {
            materialized: false,
            ..
        }
        | Statement::ShowColumns { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ShowFunctions { .. }
        | Statement::ShowStatus { global: false, .. }
        | Statement::ShowVariables { global: false, .. }
        | Statement::ShowVariable { .. }
        | Statement::ShowCollation { .. }
        | Statement::ShowCreate {
            obj_type:
                sqlparser::ast::ShowCreateObject::Table | sqlparser::ast::ShowCreateObject::View,
            ..
        } => matches!(engine, DbEngine::MariaDb | DbEngine::MySql),
        _ => false,
    }
}

fn supported_query_body(body: &sqlparser::ast::SetExpr) -> bool {
    use sqlparser::ast::SetExpr;
    match body {
        SetExpr::Select(_) | SetExpr::Query(_) | SetExpr::Values(_) => true,
        SetExpr::SetOperation { left, right, .. } => {
            supported_query_body(left) && supported_query_body(right)
        }
        SetExpr::Insert(_)
        | SetExpr::Update(_)
        | SetExpr::Delete(_)
        | SetExpr::Merge(_)
        | SetExpr::Table(_) => false,
    }
}

struct Inspect<'a> {
    engine: &'a DbEngine,
    statements: usize,
    schema_change: bool,
    footprint: Footprint,
    bindings: Vec<String>,
}

impl Inspect<'_> {
    fn qualify(&mut self, name: &ObjectName, alias: Option<&Ident>) -> ControlFlow<SqlRefusal> {
        match simple_name(name) {
            Ok(path)
                if !matches!(self.engine, DbEngine::Sqlite)
                    || path
                        .iter()
                        .all(|name| !name.value.to_ascii_lowercase().starts_with("sqlite_")) =>
            {
                self.footprint.relations.insert(Relation {
                    path,
                    alias: alias.map(Name::from),
                });
                ControlFlow::Continue(())
            }
            Ok(_) => ControlFlow::Break(SqlRefusal::UnqualifiedRelation),
            Err(refusal) => ControlFlow::Break(refusal),
        }
    }

    fn read_column(&mut self, path: Vec<Name>) {
        self.footprint.columns.insert(ColumnAccess::Named(path));
    }
}

impl Visitor for Inspect<'_> {
    type Break = SqlRefusal;

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
        self.statements += 1;
        if self.statements > 1 && !matches!(statement, Statement::Query(_)) {
            return ControlFlow::Break(SqlRefusal::WrongMode);
        }
        match statement {
            Statement::Insert(insert) => {
                if let TableObject::TableName(table) = &insert.table {
                    self.qualify(table, insert.table_alias.as_ref().map(|alias| &alias.alias))?;
                }
            }
            Statement::ExplainTable { table_name, .. } => self.qualify(table_name, None)?,
            Statement::ShowCreate { obj_name, .. } => self.qualify(obj_name, None)?,
            Statement::ShowColumns { show_options, .. } => {
                if let Some(name) = show_options
                    .show_in
                    .as_ref()
                    .and_then(|scope| scope.parent_name.as_ref())
                {
                    self.qualify(name, None)?;
                }
            }
            Statement::Drop { names, .. } => {
                for name in names {
                    self.qualify(name, None)?;
                }
            }
            Statement::AlterTable(table) => match rename_destination(table) {
                Ok(Some(path)) => {
                    self.footprint
                        .relations
                        .insert(Relation { path, alias: None });
                }
                Ok(None) => {}
                Err(error) => return ControlFlow::Break(error),
            },
            _ => {}
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<Self::Break> {
        if self.schema_change {
            self.qualify(relation, None)?;
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        if !supported_query_body(&query.body)
            || !query.locks.is_empty()
            || query.for_clause.is_some()
            || query.settings.is_some()
            || query.format_clause.is_some()
            || !query.pipe_operators.is_empty()
        {
            return ControlFlow::Break(SqlRefusal::WrongMode);
        }
        if let Some(with) = &query.with {
            for table in &with.cte_tables {
                self.footprint
                    .common_tables
                    .insert(Name::from(&table.alias.name));
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<Self::Break> {
        if select.into.is_some()
            || !select.optimizer_hints.is_empty()
            || !select.lateral_views.is_empty()
        {
            return ControlFlow::Break(SqlRefusal::WrongMode);
        }
        use sqlparser::ast::{JoinConstraint as Constraint, JoinOperator as Join};
        for join in select.from.iter().flat_map(|source| &source.joins) {
            let condition = match &join.join_operator {
                Join::Join(condition)
                | Join::Inner(condition)
                | Join::Left(condition)
                | Join::LeftOuter(condition)
                | Join::Right(condition)
                | Join::RightOuter(condition)
                | Join::FullOuter(condition)
                | Join::CrossJoin(condition)
                | Join::Semi(condition)
                | Join::LeftSemi(condition)
                | Join::RightSemi(condition)
                | Join::Anti(condition)
                | Join::LeftAnti(condition)
                | Join::RightAnti(condition)
                | Join::StraightJoin(condition) => condition,
                _ => return ControlFlow::Break(SqlRefusal::UnqualifiedRelation),
            };
            match condition {
                Constraint::Using(columns) | Constraint::UsingAlias(columns, _) => {
                    for column in columns {
                        match simple_name(column) {
                            Ok(path) if path.len() == 1 => self.read_column(path),
                            _ => return ControlFlow::Break(SqlRefusal::UnqualifiedExpression),
                        }
                    }
                }
                Constraint::Natural => {
                    self.footprint
                        .columns
                        .insert(ColumnAccess::Wildcard(Vec::new()));
                }
                _ => {}
            }
        }
        for item in &select.projection {
            let target = match item {
                SelectItem::Wildcard(_) => Some(Vec::new()),
                SelectItem::QualifiedWildcard(
                    SelectItemQualifiedWildcardKind::ObjectName(name),
                    _,
                ) => match simple_name(name) {
                    Ok(parts) => Some(parts),
                    Err(error) => return ControlFlow::Break(error),
                },
                SelectItem::QualifiedWildcard(_, _) => {
                    return ControlFlow::Break(SqlRefusal::UnqualifiedExpression);
                }
                _ => None,
            };
            if let Some(target) = target {
                self.footprint
                    .columns
                    .insert(ColumnAccess::Wildcard(target));
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
        match factor {
            TableFactor::Table {
                name,
                alias,
                args: None,
                with_hints,
                version: None,
                json_path: None,
                sample: None,
                with_ordinality: false,
                ..
            } if with_hints.is_empty() => {
                if let Some(alias) = alias
                    && !alias.columns.is_empty()
                {
                    self.footprint
                        .columns
                        .insert(ColumnAccess::Wildcard(vec![Name::from(&alias.name)]));
                }
                self.qualify(name, alias.as_ref().map(|alias| &alias.name))
            }
            TableFactor::Derived {
                alias,
                sample: None,
                ..
            } => {
                if let Some(alias) = alias {
                    self.footprint
                        .derived_aliases
                        .insert(Name::from(&alias.name));
                }
                ControlFlow::Continue(())
            }
            TableFactor::NestedJoin { alias, .. } => {
                if let Some(alias) = alias {
                    self.footprint
                        .derived_aliases
                        .insert(Name::from(&alias.name));
                }
                ControlFlow::Continue(())
            }
            _ => ControlFlow::Break(SqlRefusal::UnqualifiedRelation),
        }
    }

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
        match expression {
            Expr::Identifier(name)
                if matches!(self.engine, DbEngine::SqlServer)
                    && name.quote_style.is_none()
                    && name.value.starts_with('@') =>
            {
                self.bindings.push(name.value.clone());
            }
            Expr::Identifier(name) if name.quote_style.is_none() && name.value.starts_with('@') => {
                return ControlFlow::Break(SqlRefusal::UnqualifiedExpression);
            }
            Expr::Identifier(name) => self.read_column(vec![Name::from(name)]),
            Expr::CompoundIdentifier(parts) => {
                self.read_column(parts.iter().map(Name::from).collect());
            }
            Expr::Cast {
                data_type: sqlparser::ast::DataType::Custom(..),
                ..
            } => return ControlFlow::Break(SqlRefusal::UnqualifiedExpression),
            Expr::Function(call) => {
                let Ok(name) = simple_name(&call.name) else {
                    return ControlFlow::Break(SqlRefusal::UnqualifiedExpression);
                };
                let builtin = match name.as_slice() {
                    [name] if !name.quoted => Some(name.value.to_ascii_lowercase()),
                    [catalog, name]
                        if catalog.value == "pg_catalog"
                            && matches!(self.engine, DbEngine::Postgres)
                            && !name.quoted =>
                    {
                        Some(name.value.to_ascii_lowercase())
                    }
                    _ => None,
                };
                if !builtin.as_deref().is_some_and(safe_builtin) {
                    return ControlFlow::Break(SqlRefusal::UnqualifiedExpression);
                }
            }
            Expr::Value(ValueWithSpan {
                value: Value::Placeholder(name),
                ..
            }) => {
                self.bindings.push(name.clone());
            }
            Expr::BinaryOp {
                op:
                    sqlparser::ast::BinaryOperator::Assignment
                    | sqlparser::ast::BinaryOperator::Custom(_)
                    | sqlparser::ast::BinaryOperator::PGCustomBinaryOperator(_),
                ..
            } => return ControlFlow::Break(SqlRefusal::UnqualifiedExpression),
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

struct QualifyPostgresBuiltins;

impl VisitorMut for QualifyPostgresBuiltins {
    type Break = ();

    fn pre_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<Self::Break> {
        if let Expr::Function(call) = expression
            && let [name] = call.name.0.as_slice()
            && let Some(name) = name.as_ident()
        {
            let function = name.value.to_ascii_lowercase();
            // These grammars are parsed by PostgreSQL itself and are not routine lookups.
            if !matches!(
                function.as_str(),
                "coalesce" | "nullif" | "greatest" | "least" | "substring" | "trim"
            ) {
                call.name.0.insert(
                    0,
                    sqlparser::ast::ObjectNamePart::Identifier(Ident::new("pg_catalog")),
                );
            }
        }
        ControlFlow::Continue(())
    }
}

pub fn safe_builtin(name: &str) -> bool {
    matches!(
        name,
        "abs"
            | "avg"
            | "ceil"
            | "ceiling"
            | "char_length"
            | "character_length"
            | "coalesce"
            | "concat"
            | "count"
            | "floor"
            | "length"
            | "len"
            | "lower"
            | "ltrim"
            | "max"
            | "min"
            | "nullif"
            | "replace"
            | "round"
            | "rtrim"
            | "substr"
            | "substring"
            | "sum"
            | "trim"
            | "upper"
    )
}

/// Relations named by the schema operation, including any bound read or write dependencies.
pub fn schema_targets(statements: &[CheckedStatement]) -> Vec<Relation> {
    statements
        .iter()
        .flat_map(|statement| statement.read.relations.iter())
        .map(|relation| Relation {
            path: relation.path.clone(),
            alias: None,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub fn check(
    engine: &DbEngine,
    mode: DataMode,
    source: &str,
) -> Result<Vec<CheckedStatement>, SqlRefusal> {
    if source.is_empty() || source.len() > MAX_SQL_BYTES || source.contains('\0') {
        return Err(SqlRefusal::InvalidStatement);
    }
    let parsed =
        Parser::parse_sql(dialect(engine), source).map_err(|_| SqlRefusal::InvalidStatement)?;
    if parsed.is_empty()
        || (mode != DataMode::Migration && parsed.len() != 1)
        || parsed.len() > MAX_MIGRATION_STATEMENTS
    {
        return Err(SqlRefusal::StatementCount);
    }
    parsed
        .into_iter()
        .map(|mut statement| {
            let schema_change = schema_statement(&statement, engine);
            let accepted = match mode {
                DataMode::Read => read_statement(&statement, engine),
                DataMode::Write => write_statement(&statement),
                DataMode::Schema => schema_change,
                DataMode::Migration => schema_change || write_statement(&statement),
            };
            if !accepted {
                return Err(SqlRefusal::WrongMode);
            }
            let mut inspect = Inspect {
                engine,
                statements: 0,
                schema_change,
                footprint: Footprint::default(),
                bindings: Vec::new(),
            };
            if let ControlFlow::Break(refusal) = statement.visit(&mut inspect) {
                return Err(refusal);
            }
            if mode == DataMode::Migration && !inspect.bindings.is_empty() {
                return Err(SqlRefusal::InvalidBinding);
            }
            let inspection = match &statement {
                Statement::ShowTables { show_options, .. }
                | Statement::ShowViews { show_options, .. } => {
                    let scope = show_options
                        .show_in
                        .as_ref()
                        .and_then(|scope| scope.parent_name.as_ref());
                    Some(Inspection::TableListing {
                        catalog: scope.map(simple_name).transpose()?,
                        views_only: matches!(statement, Statement::ShowViews { .. }),
                    })
                }
                Statement::ShowDatabases { .. } | Statement::ShowSchemas { .. } => {
                    Some(Inspection::CatalogListing)
                }
                Statement::ShowFunctions { .. } => Some(Inspection::RoutineListing),
                Statement::ShowVariables { .. } => Some(Inspection::Variables(None)),
                Statement::ShowVariable { variable } => {
                    let [name] = variable.as_slice() else {
                        return Err(SqlRefusal::UnqualifiedExpression);
                    };
                    if name.value.is_empty()
                        || !name
                            .value
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    {
                        return Err(SqlRefusal::UnqualifiedExpression);
                    }
                    Some(Inspection::Variables(Some(name.value.to_ascii_lowercase())))
                }
                Statement::ShowCreate { .. }
                | Statement::ShowColumns { .. }
                | Statement::ExplainTable { .. } => Some(Inspection::TableDefinition),
                Statement::Explain { .. } => Some(Inspection::QueryPlan),
                _ => None,
            };
            if matches!(engine, DbEngine::Postgres) {
                let _ = VisitMut::visit(&mut statement, &mut QualifyPostgresBuiltins);
            }
            let renames_schema_identity = match &statement {
                Statement::AlterTable(table) => table.operations.iter().any(|change| {
                    matches!(
                        change,
                        sqlparser::ast::AlterTableOperation::RenameColumn { .. }
                            | sqlparser::ast::AlterTableOperation::RenameTable { .. }
                            | sqlparser::ast::AlterTableOperation::ChangeColumn { .. }
                    )
                }),
                _ => false,
            };
            let created_relation = match &statement {
                Statement::CreateTable(table) => Some(simple_name(&table.name)?),
                Statement::CreateView(view) => Some(simple_name(&view.name)?),
                Statement::AlterTable(table) => rename_destination(table)?,
                _ => None,
            };
            let sql = match &statement {
                Statement::ShowViews { show_options, .. } => {
                    format!("SHOW FULL TABLES{show_options}")
                }
                Statement::ShowFunctions { filter } => filter.as_ref().map_or_else(
                    || "SHOW FUNCTION STATUS".to_owned(),
                    |filter| format!("SHOW FUNCTION STATUS {filter}"),
                ),
                Statement::ShowVariable { .. } => "SHOW SESSION VARIABLES".to_owned(),
                _ => statement.to_string(),
            };
            Ok(CheckedStatement {
                inspection,
                sql,
                read: inspect.footprint,
                bindings: inspect.bindings,
                schema_change,
                renames_schema_identity,
                created_relation,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separates_query_data_change_and_schema_change() {
        let engine = DbEngine::Postgres;
        assert!(check(&engine, DataMode::Read, "SELECT id FROM contacts").is_ok());
        assert!(matches!(
            check(&engine, DataMode::Read, "UPDATE contacts SET active = TRUE"),
            Err(SqlRefusal::WrongMode)
        ));
        assert!(matches!(
            check(&engine, DataMode::Write, "CREATE TABLE contacts (id INT)"),
            Err(SqlRefusal::WrongMode)
        ));
        assert!(check(&engine, DataMode::Schema, "CREATE TABLE contacts (id INT)").is_ok());
        assert!(matches!(
            check(&engine, DataMode::Read, "SELECT 1; SELECT 2"),
            Err(SqlRefusal::StatementCount)
        ));
    }

    #[test]
    fn refusals_and_schema_targets_preserve_authorized_footprints() {
        let engine = DbEngine::Postgres;
        for sql in [
            "TABLE contacts",
            "SELECT id FROM contacts UNION TABLE contacts",
        ] {
            assert!(check(&engine, DataMode::Read, sql).is_err(), "{sql}");
        }
        let plans = check(
            &engine,
            DataMode::Read,
            "SELECT c.id FROM public.contacts AS c(id, name)",
        )
        .unwrap();
        assert!(
            plans[0]
                .read
                .columns
                .iter()
                .any(|column| matches!(column, ColumnAccess::Wildcard(_)))
        );

        let plans = check(
            &engine,
            DataMode::Migration,
            "ALTER TABLE public.contacts RENAME COLUMN name TO label; INSERT INTO public.contacts (id) VALUES (1)",
        )
        .unwrap();
        let targets = schema_targets(&plans);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].path[1].value, "contacts");
        assert!(plans[0].renames_schema_identity);
        let rename = check(
            &engine,
            DataMode::Schema,
            "ALTER TABLE public.contacts RENAME TO archived",
        )
        .unwrap();
        let targets = schema_targets(&rename);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].path[1].value, "archived");
        assert_eq!(targets[1].path[1].value, "contacts");
    }

    #[test]
    fn records_aliases_column_use_wildcards_and_native_bindings() {
        let plans = check(
            &DbEngine::Postgres,
            DataMode::Read,
            "SELECT c.*, LENGTH(c.name) FROM public.contacts AS c WHERE c.id = $1",
        )
        .unwrap();
        let statement = &plans[0];
        assert!(statement.read.relations.iter().any(|relation| {
            relation
                .alias
                .as_ref()
                .is_some_and(|alias| alias.value == "c")
        }));
        assert!(statement.read.columns.iter().any(|column| {
            matches!(column, ColumnAccess::Named(parts) if parts.last().is_some_and(|part| part.value == "name"))
        }));
        assert!(
            statement
                .read
                .columns
                .iter()
                .any(|column| matches!(column, ColumnAccess::Wildcard(_)))
        );
        assert_eq!(statement.bindings, ["$1"]);
    }
}
