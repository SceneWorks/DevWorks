//! Domain extractor golden inputs: the retained CodeGraph SQL (`SqlExtractor`/`SqlGraphVisitor`),
//! Terraform (`HclParser`), Ansible (`AnsibleParser`) and ColdFusion (`ColdFusionParser`) output,
//! node labels, qualified names (without the repository prefix), properties and edges, including
//! the retained quirks (positive and negative).
use devworks_adapters::index::treesitter::TreeSitterParser;
use devworks_core::ports::indexing::{
    EdgeTarget, ExtractedEdge, ExtractedNode, FileExtract, Resolution, StructuralParser,
};
use serde_json::{Value, json};

fn extract(path: &str, src: &str) -> FileExtract {
    let p = TreeSitterParser::new();
    let lang = p.language_of(path).expect("a language").id;
    p.extract(lang, path, "repo", src.as_bytes(), &|| false, 5_000)
        .expect("extracts")
}
fn summary(x: &FileExtract) -> Vec<(String, String)> {
    x.nodes
        .iter()
        .map(|n| (n.kind.clone(), n.qualified_name.clone()))
        .collect()
}
fn node<'x>(x: &'x FileExtract, kind: &str, qn: &str) -> (usize, &'x ExtractedNode) {
    let found: Vec<(usize, &ExtractedNode)> = x
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.kind == kind && n.qualified_name == qn)
        .collect();
    assert_eq!(found.len(), 1, "one {kind} {qn}; have {:?}", summary(x));
    found[0]
}
fn no_node(x: &FileExtract, qn: &str) {
    assert!(
        !x.nodes.iter().any(|n| n.qualified_name == qn),
        "unexpected {qn}: {:?}",
        summary(x)
    );
}
fn prop<'n>(n: &'n ExtractedNode, key: &str) -> &'n Value {
    n.properties
        .get(key)
        .unwrap_or_else(|| panic!("{} has no {key}: {:?}", n.qualified_name, n.properties))
}
fn edges<'x>(
    x: &'x FileExtract,
    kind: &str,
    from: usize,
    to: &EdgeTarget,
) -> Vec<&'x ExtractedEdge> {
    x.edges
        .iter()
        .filter(|e| e.kind == kind && e.source == from && &e.target == to)
        .collect()
}
/// The one `kind` edge from `from` to the local node `to`.
fn local<'x>(x: &'x FileExtract, kind: &str, from: usize, to: usize) -> &'x ExtractedEdge {
    let e = edges(x, kind, from, &EdgeTarget::Local(to));
    assert_eq!(e.len(), 1, "one {kind} {from}->{to}; have {:?}", x.edges);
    e[0]
}
/// The one `kind` edge from `from` to the symbol `name`.
fn symbol<'x>(x: &'x FileExtract, kind: &str, from: usize, name: &str) -> &'x ExtractedEdge {
    let e = edges(x, kind, from, &EdgeTarget::Symbol(name.into()));
    assert_eq!(e.len(), 1, "one {kind} {from}->{name}; have {:?}", x.edges);
    assert_eq!(e[0].resolution, Resolution::Unresolved);
    e[0]
}
fn no_symbol(x: &FileExtract, kind: &str, name: &str) {
    assert!(
        !x.edges
            .iter()
            .any(|e| e.kind == kind && e.target == EdgeTarget::Symbol(name.into())),
        "unexpected {kind} {name}"
    );
}
fn p(e: &ExtractedEdge, key: &str) -> Value {
    e.properties.get(key).cloned().unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------------------------
// Terraform

const MAIN_TF: &str = r#"resource "aws_ecs_service" "api" {
  name       = "api-svc"
  depends_on = [aws_iam_role.exec, module.vpc]
  tags = {
    Name = "api"
  }
}

resource "aws_iam_role" "exec" {
  name = "exec-role"
}

data "aws_ami" "ubuntu" {
  most_recent = true
}

module "vpc" {
  source  = "./modules/vpc"
  version = "1.2.0"
}

module "db" {
  source = "terraform-aws-modules/rds/aws"
}

variable "region" {
  type        = string
  default     = "us-east-1"
  description = "AWS region"
}

output "role_arn" {
  description = "The role"
  value       = aws_iam_role.exec.arn
}

locals {
  env = "prod"
  url = "https://api.example.com/v1"
}

provider "aws" {
  region = var.region
}

resource "aws_lambda_function" "worker" {
  function_name = "worker-fn"
  handler       = "index.handler"
  runtime       = "nodejs20.x"
}

resource "aws_db_instance" "main" {
  engine  = "postgres"
  db_name = "app"
}
"#;

#[test]
fn terraform_resources_data_modules_variables_outputs_locals() {
    let x = extract("infra/main.tf", MAIN_TF);
    assert_eq!(x.nodes[0].kind, "File");

    let (api, n) = node(&x, "TerraformResource", "resource.aws_ecs_service.api");
    assert_eq!(n.name, "aws_ecs_service.api");
    assert_eq!(n.span.start_line, 1);
    assert_eq!(n.span.end_line, 7);
    assert_eq!(prop(n, "resource_type"), "aws_ecs_service");
    assert_eq!(prop(n, "provider"), "aws");
    assert_eq!(prop(n, "confidence"), "high");
    assert_eq!(prop(n, "display_name"), "api-svc");
    assert_eq!(prop(n, "tag_name"), "api");
    let (exec, n) = node(&x, "TerraformResource", "resource.aws_iam_role.exec");
    assert_eq!(prop(n, "display_name"), "exec-role");

    let (_, d) = node(&x, "TerraformDataSource", "data.aws_ami.ubuntu");
    assert_eq!(d.name, "data.aws_ami.ubuntu");
    assert_eq!(prop(d, "data_type"), "aws_ami");
    assert_eq!(prop(d, "provider"), "aws");

    let (vpc, m) = node(&x, "TerraformModule", "module.vpc");
    assert_eq!(m.name, "vpc");
    assert_eq!(prop(m, "source"), "./modules/vpc");
    assert_eq!(prop(m, "version"), "1.2.0");
    let e = symbol(&x, "INCLUDES_MODULE", vpc, "module_source.modules_vpc");
    assert_eq!(p(e, "source"), "./modules/vpc");
    let (db, _) = node(&x, "TerraformModule", "module.db");
    symbol(
        &x,
        "INCLUDES_MODULE",
        db,
        "terraform_registry:terraform-aws-modules/rds/aws",
    );

    let (_, v) = node(&x, "TerraformVariable", "var.region");
    assert_eq!(prop(v, "type"), "string");
    assert_eq!(prop(v, "default_value"), "us-east-1");
    assert_eq!(prop(v, "description"), "AWS region");

    let (out, o) = node(&x, "TerraformOutput", "output.role_arn");
    assert_eq!(prop(o, "description"), "The role");
    assert_eq!(prop(o, "value_expression"), "aws_iam_role.exec.arn");
    let calls = local(&x, "CALLS", out, exec);
    assert_eq!(calls.resolution, Resolution::Inferred);

    for local_name in ["env", "url"] {
        let (_, l) = node(&x, "TerraformVariable", &format!("local.{local_name}"));
        assert_eq!(prop(l, "scope"), "local");
    }
    // The retained parser has no provider blocks.
    assert!(
        !x.nodes
            .iter()
            .any(|n| n.qualified_name.contains("provider"))
    );

    // depends_on: resource and module references; `module.vpc` also matches the resource
    // pattern (a retained quirk).
    local(&x, "DEPENDS_ON", api, exec);
    local(&x, "DEPENDS_ON", api, vpc);
    symbol(&x, "DEPENDS_ON", api, "resource.module.vpc");

    // Cross-repository targets by resource type.
    let e = symbol(&x, "DEPLOYS", api, "api-svc");
    assert_eq!(p(e, "container"), "api-svc");
    assert_eq!(p(e, "confidence"), "medium");
    assert!((e.confidence - 0.6).abs() < 1e-6);
    let (worker, w) = node(
        &x,
        "TerraformResource",
        "resource.aws_lambda_function.worker",
    );
    assert_eq!(prop(w, "handler"), "index.handler");
    assert_eq!(prop(w, "runtime"), "nodejs20.x");
    let e = symbol(&x, "DEPLOYS", worker, "worker-fn");
    assert_eq!(p(e, "function_name"), "worker-fn");
    let (rds, r) = node(&x, "TerraformResource", "resource.aws_db_instance.main");
    assert_eq!(prop(r, "engine"), "postgres");
    assert_eq!(prop(r, "database_name"), "app");
    assert_eq!(p(symbol(&x, "CONFIGURES", rds, "app"), "database"), "app");
    assert!(
        edges(&x, "DEPLOYS", exec, &EdgeTarget::Symbol("exec-role".into())).is_empty(),
        "an IAM role has no cross-repository target"
    );

    // String-literal references from the file.
    let e = symbol(&x, "CALLS", 0, "https://api.example.com/v1");
    assert_eq!(p(e, "receiver_type"), "terraform_url_ref");
    assert!((e.confidence - 0.4).abs() < 1e-6);
    assert!(e.properties.get("source_qualified_name").is_none());
    let e = symbol(&x, "CALLS", 0, "api.example.com");
    assert_eq!(p(e, "receiver_type"), "terraform_host_ref");
}

#[test]
fn terraform_tfvars_and_connection_references() {
    let x = extract(
        "env/prod.tfvars",
        "region = \"us-west-2\"\ndb_url = \"postgres://db.internal:5432/app\"\nconn = \"Server=sql01.corp;Database=x\"\n",
    );
    let (_, r) = node(&x, "TerraformVariable", "tfvar.region");
    assert_eq!(prop(r, "scope"), "tfvar");
    assert_eq!(prop(r, "value_preview"), "us-west-2");
    assert!(r.properties.get("likely_service_ref").is_none());
    let (_, d) = node(&x, "TerraformVariable", "tfvar.db_url");
    assert_eq!(prop(d, "value_preview"), "postgres://db.internal:5432/app");
    assert_eq!(prop(d, "likely_service_ref"), true);
    assert_eq!(
        p(symbol(&x, "CALLS", 0, "db.internal"), "receiver_type"),
        "terraform_host_ref"
    );
    // `Server=sql01.corp`: a host and a connection-string reference (deduplicated per kind, not
    // across kinds).
    let sql01 = edges(&x, "CALLS", 0, &EdgeTarget::Symbol("sql01.corp".into()));
    let mut kinds: Vec<Value> = sql01.iter().map(|e| p(e, "receiver_type")).collect();
    kinds.sort_by_key(|k| k.to_string());
    assert_eq!(
        kinds,
        vec![
            json!("terraform_connection_ref"),
            json!("terraform_host_ref")
        ]
    );
    // No `.tf` blocks in a tfvars file.
    assert!(
        x.nodes
            .iter()
            .skip(1)
            .all(|n| n.kind == "TerraformVariable")
    );
}

// ---------------------------------------------------------------------------------------------
// SQL

const SCHEMA_SQL: &str = "CREATE TABLE dbo.Orgs (
    Id INT NOT NULL,
    Name NVARCHAR(100) NULL,
    CONSTRAINT PK_Orgs PRIMARY KEY (Id)
);
GO
CREATE TABLE dbo.Users (
    Id INT IDENTITY(1,1) PRIMARY KEY,
    OrgId INT NOT NULL REFERENCES dbo.Orgs(Id),
    Email VARCHAR(255),
    Score DECIMAL(10, 2),
    Total AS (Score * 2),
    CONSTRAINT FK_Users_Orgs FOREIGN KEY (OrgId) REFERENCES dbo.Orgs (Id)
);
GO
ALTER TABLE dbo.Users WITH CHECK ADD CONSTRAINT FK_Users_Mgr FOREIGN KEY (ManagerId) REFERENCES dbo.Managers (Id);
GO
CREATE UNIQUE NONCLUSTERED INDEX IX_Users_Email ON dbo.Users (Email ASC, OrgId);
GO
CREATE VIEW dbo.ActiveUsers AS
SELECT u.Id, o.Name FROM dbo.Users u JOIN dbo.Orgs o ON o.Id = u.OrgId, #tmp t;
GO
CREATE PROCEDURE dbo.GetUser @Id INT, @Name NVARCHAR(50) = NULL OUTPUT
AS
BEGIN
    SELECT * FROM dbo.Users WHERE Id = @Id;
    UPDATE dbo.Audit SET Seen = 1;
    INSERT INTO dbo.Log (Msg) VALUES ('x');
    DELETE FROM #work;
    EXEC dbo.Notify @Id;
    EXEC sp_executesql N'SELECT 1';
    EXEC (@sql);
    INSERT INTO dbo.Staging EXEC dbo.Load;
END
GO
CREATE FUNCTION dbo.Score (@UserId INT) RETURNS INT AS
BEGIN
    RETURN (SELECT COUNT(*) FROM dbo.Users WHERE Id = @UserId);
END
GO
CREATE OR ALTER VIEW dbo.Skipped AS SELECT 1 AS x FROM dbo.Archive;
GO
CREATE OR ALTER PROCEDURE dbo.SkippedProc AS
BEGIN
    SELECT * FROM dbo.ArchiveProc;
    EXEC dbo.NotifyArchive;
END
GO
CREATE OR ALTER FUNCTION dbo.SkippedFn () RETURNS INT AS
BEGIN
    RETURN (SELECT COUNT(*) FROM dbo.ArchiveFn);
END
";

fn column<'v>(table: &'v ExtractedNode, name: &str) -> &'v Value {
    prop(table, "columns")
        .as_array()
        .expect("columns")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no column {name}"))
}

#[test]
fn sql_tables_columns_keys_and_indexes() {
    let x = extract("db/schema.sql", SCHEMA_SQL);
    assert_eq!(
        x.parse_errors, 0,
        "a text scanner reports no grammar errors"
    );
    let (orgs, o) = node(&x, "Table", "dbo.Orgs");
    assert_eq!(o.name, "dbo.Orgs");
    assert_eq!((o.span.start_line, o.span.end_line), (1, 5));
    assert_eq!(prop(o, "column_count"), 2);
    assert_eq!(
        column(o, "Id"),
        &json!({"name": "Id", "type": "INT", "nullable": false, "is_primary_key": true})
    );
    assert_eq!(
        column(o, "Name"),
        &json!({"name": "Name", "type": "NVARCHAR(100)", "nullable": true})
    );

    let (users, u) = node(&x, "Table", "dbo.Users");
    assert_eq!(prop(u, "column_count"), 5);
    // An inline primary key without NOT NULL stays nullable (the retained check).
    assert_eq!(
        column(u, "Id"),
        &json!({"name": "Id", "type": "INT", "nullable": true, "is_primary_key": true})
    );
    assert_eq!(column(u, "OrgId")["nullable"], false);
    assert_eq!(column(u, "Email")["type"], "VARCHAR(255)");
    assert_eq!(column(u, "Score")["type"], "DECIMAL(10, 2)");
    assert_eq!(column(u, "Total")["type"], "unknown", "a computed column");

    let fks = edges(&x, "QUERIES", users, &EdgeTarget::Local(orgs));
    assert_eq!(fks.len(), 2, "inline and table-level foreign keys");
    assert!(fks.iter().all(|e| e.resolution == Resolution::Inferred));
    assert!(fks.iter().any(|e| p(e, "relationship") == "foreign_key"
        && p(e, "column") == "OrgId"
        && p(e, "referenced_column") == "Id"));
    assert!(
        fks.iter()
            .any(|e| p(e, "columns") == "OrgId" && p(e, "referenced_columns") == "Id")
    );

    let e = symbol(&x, "QUERIES", users, "dbo.Managers");
    assert_eq!(p(e, "relationship"), "foreign_key");
    assert_eq!(p(e, "via"), "alter_table");

    let e = local(&x, "DEFINES", users, users);
    assert_eq!(p(e, "relationship"), "index");
    assert_eq!(p(e, "index_name"), "IX_Users_Email");
    assert_eq!(p(e, "columns"), "Email, OrgId");
    assert_eq!(p(e, "is_unique"), true);
}

#[test]
fn sql_views_procedures_and_functions() {
    let x = extract("db/schema.sql", SCHEMA_SQL);
    let (users, _) = node(&x, "Table", "dbo.Users");
    let (orgs, _) = node(&x, "Table", "dbo.Orgs");

    let (view, v) = node(&x, "View", "dbo.ActiveUsers");
    assert!(v.properties.is_empty());
    for t in [users, orgs] {
        assert_eq!(p(local(&x, "QUERIES", view, t), "confidence_band"), "high");
    }
    no_symbol(&x, "QUERIES", "#tmp");

    let (proc_, pr) = node(&x, "StoredProcedure", "dbo.GetUser");
    assert_eq!(
        prop(pr, "parameters"),
        &json!([
            {"name": "@Id", "type": "INT", "is_output": false},
            {"name": "@Name", "type": "NVARCHAR(50)", "is_output": true}
        ])
    );
    assert_eq!(prop(pr, "parameter_count"), 2);
    local(&x, "QUERIES", proc_, users);
    for t in ["dbo.Audit", "dbo.Log", "dbo.Staging"] {
        assert_eq!(
            p(symbol(&x, "QUERIES", proc_, t), "confidence_band"),
            "high"
        );
    }
    no_symbol(&x, "QUERIES", "#work");
    symbol(&x, "CALLS", proc_, "dbo.Notify");
    symbol(&x, "CALLS", proc_, "sp_executesql");
    no_symbol(&x, "CALLS", "dbo.Load");

    let (func, f) = node(&x, "Function", "dbo.Score");
    assert_eq!(
        prop(f, "parameters"),
        &json!([{"name": "@UserId", "type": "INT"}])
    );
    assert_eq!(prop(f, "parameter_count"), 1);
    assert_eq!(prop(f, "kind"), "sql_function");
    assert!(
        x.edges.iter().all(|e| e.source != func),
        "the retained visitor never walked a function body as the function"
    );

    // CREATE OR ALTER is not the visited CreateViewStatement (nor CreateProcedureStatement or
    // CreateFunctionStatement): no node, and its body is walked with no current object.
    no_node(&x, "dbo.Skipped");
    no_symbol(&x, "QUERIES", "dbo.Archive");
    no_node(&x, "dbo.SkippedProc");
    no_symbol(&x, "QUERIES", "dbo.ArchiveProc");
    no_symbol(&x, "CALLS", "dbo.NotifyArchive");
    no_node(&x, "dbo.SkippedFn");
    no_symbol(&x, "QUERIES", "dbo.ArchiveFn");
}

#[test]
fn sql_mysql_syntax_is_normalized_first() {
    let x = extract(
        "db/mysql.sql",
        "# a MySQL comment\nCREATE TABLE IF NOT EXISTS `users` (\n  `id` INT(11) UNSIGNED NOT NULL AUTO_INCREMENT,\n  `active` BOOLEAN DEFAULT 1,\n  `role` ENUM('a','b') NOT NULL,\n  `bio` LONGTEXT,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;\nCREATE TABLE `posts` (`id` INT, `user_id` INT, FOREIGN KEY (`user_id`) REFERENCES `users`(`id`));\n",
    );
    let (users, u) = node(&x, "Table", "users");
    assert_eq!((u.span.start_line, u.span.end_line), (2, 8));
    assert_eq!(
        column(u, "id"),
        &json!({"name": "id", "type": "INT", "nullable": false, "is_primary_key": true})
    );
    assert_eq!(column(u, "active")["type"], "BIT");
    assert_eq!(column(u, "role")["type"], "VARCHAR(255)");
    assert_eq!(column(u, "bio")["type"], "NVARCHAR(MAX)");
    let (posts, _) = node(&x, "Table", "posts");
    let e = local(&x, "QUERIES", posts, users);
    assert_eq!(p(e, "columns"), "user_id");
    assert_eq!(p(e, "referenced_columns"), "id");
}

// ---------------------------------------------------------------------------------------------
// Ansible

const SITE_YML: &str = r#"---
- name: Web servers
  hosts: webservers
  vars:
    api_url: "http://api.internal:8080/v1"
  roles:
    - common
    - role: nginx
  pre_tasks:
    - name: Update cache
      apt:
        update_cache: yes
  tasks:
    - name: Install App
      apt:
        name: app
      notify:
        - Restart App
    - block:
        - name: Deploy config
          template:
            src: app.conf.j2
            dest: /etc/app.conf
      rescue:
        - name: Report
          debug:
            msg: failed
    - name: Include extra
      include_tasks: extra.yml
    - name: Include role
      include_role:
        name: monitoring
    - name: Start service
      service:
        name: app
        state: started
  handlers:
    - name: Restart App
      service:
        name: app
        state: restarted
      listen: restart web
"#;

#[test]
fn ansible_playbook_plays_tasks_handlers_roles_and_includes() {
    let x = extract("site.yml", SITE_YML);
    assert!(x.incomplete.is_empty(), "{:?}", x.incomplete);
    let (pb, n) = node(&x, "Playbook", "playbook.site");
    assert_eq!(n.name, "site");
    assert_eq!(prop(n, "type"), "playbook");

    for role in ["common", "nginx"] {
        let (r, rn) = node(&x, "Role", &format!("role.{role}"));
        assert_eq!(prop(rn, "confidence"), "medium");
        assert_eq!(
            local(&x, "INCLUDES_ROLE", pb, r).resolution,
            Resolution::Inferred
        );
    }

    let task = |qn: &str| node(&x, "AnsibleTask", &format!("playbook.site.task.{qn}"));
    let (install, t) = task("install_app");
    assert_eq!(t.name, "Install App");
    assert_eq!(t.parent, Some(pb));
    assert_eq!(prop(t, "module"), "apt");
    assert_eq!(prop(t, "confidence"), "high");
    let d = local(&x, "DEFINES", pb, install);
    assert_eq!((d.resolution, d.confidence), (Resolution::Resolved, 1.0));
    let (_, t) = task("update_cache");
    assert_eq!(prop(t, "module"), "apt");
    // block/rescue children are tasks of the play.
    let (deploy, t) = task("deploy_config");
    assert_eq!(prop(t, "module"), "template");
    local(&x, "DEFINES", pb, deploy);
    let (_, t) = task("report");
    assert_eq!(prop(t, "module"), "debug");
    no_node(&x, "playbook.site.task.unnamed_task");

    let (include, t) = task("include_extra");
    assert!(
        t.properties.get("module").is_none(),
        "include_tasks is not a module"
    );
    symbol(&x, "CALLS", include, "playbook.site.taskfile.extra");
    let (inc_role, _) = task("include_role");
    let (mon, m) = node(&x, "Role", "role.monitoring");
    assert!(m.properties.is_empty());
    local(&x, "INCLUDES_ROLE", inc_role, mon);

    let (handler, h) = node(&x, "AnsibleHandler", "handler.restart_app");
    assert_eq!(h.name, "Restart App");
    assert_eq!(prop(h, "module"), "service");
    assert_eq!(prop(h, "listen"), "restart web");
    assert_eq!(h.parent, Some(0));
    assert_eq!(
        local(&x, "NOTIFIES_HANDLER", install, handler).resolution,
        Resolution::Inferred
    );

    // Cross-repository targets of known modules.
    let (start, _) = task("start_service");
    let e = symbol(&x, "DEPLOYS", start, "app");
    assert_eq!(p(e, "service_name"), "app");
    assert_eq!(p(e, "confidence"), "medium");
    symbol(&x, "DEPLOYS", handler, "app");
    let e = symbol(&x, "CONFIGURES", deploy, "/etc/app.conf");
    assert_eq!(p(e, "destination"), "/etc/app.conf");

    // The play's hosts, and references in scalars (from the task-file parent, not a node here).
    let e = symbol(&x, "CALLS", pb, "webservers");
    assert_eq!(p(e, "receiver_type"), "ansible_hosts");
    assert!((e.confidence - 0.5).abs() < 1e-6);
    let e = symbol(&x, "CALLS", 0, "http://api.internal:8080/v1");
    assert_eq!(p(e, "receiver_type"), "ansible_url_ref");
    assert_eq!(p(e, "source_qualified_name"), "taskfile.site");
    assert_eq!(
        p(symbol(&x, "CALLS", 0, "api.internal"), "receiver_type"),
        "ansible_host_ref"
    );
}

#[test]
fn ansible_role_directory_files() {
    let tasks = extract(
        "roles/web/tasks/main.yml",
        "- name: Copy file\n  copy:\n    src: a\n    dest: /opt/a\n- name: Include more\n  import_tasks: more.yml\n- name: Queue\n  shell: echo\n  environment:\n    queue_name: orders.created\n",
    );
    let (copy, t) = node(&tasks, "AnsibleTask", "role.web.task.copy_file");
    assert_eq!(prop(t, "module"), "copy");
    assert_eq!(t.parent, Some(0));
    // The role node lives in meta/main.yml: the file stands for it here.
    let d = local(&tasks, "DEFINES", 0, copy);
    assert_eq!(d.resolution, Resolution::Resolved);
    assert_eq!(p(d, "source_qualified_name"), "role.web");
    symbol(&tasks, "CONFIGURES", copy, "/opt/a");
    let (more, _) = node(&tasks, "AnsibleTask", "role.web.task.include_more");
    symbol(&tasks, "CALLS", more, "role.web.taskfile.more");
    let e = symbol(&tasks, "CALLS", 0, "orders.created");
    assert_eq!(p(e, "receiver_type"), "ansible_queue_ref");

    let meta = extract(
        "roles/web/meta/main.yml",
        "galaxy_info:\n  description: Web role\ndependencies:\n  - common\n  - role: base\n",
    );
    let (web, r) = node(&meta, "Role", "role.web");
    assert_eq!(prop(r, "confidence"), "high");
    assert_eq!(prop(r, "description"), "Web role");
    for dep in ["common", "base"] {
        let (d, dn) = node(&meta, "Role", &format!("role.{dep}"));
        assert!(dn.properties.is_empty());
        local(&meta, "INCLUDES_ROLE", web, d);
    }

    let defaults = extract(
        "roles/web/defaults/main.yml",
        "port: 8080\ndb_url: \"postgres://db:5432/app\"\nlong: \"",
    );
    assert_eq!(defaults.nodes.len(), 1, "a syntax error fails the file");
    assert!(!defaults.incomplete.is_empty());
    let defaults = extract(
        "roles/web/defaults/main.yml",
        "port: 8080\ndb_url: \"postgres://db:5432/app\"\nlist: [1, 2]\n",
    );
    let (_, v) = node(&defaults, "AnsibleVariable", "default.web.port");
    assert_eq!(prop(v, "scope"), "default");
    assert_eq!(prop(v, "value_preview"), "8080");
    let (_, v) = node(&defaults, "AnsibleVariable", "default.web.db_url");
    assert_eq!(prop(v, "likely_service_ref"), true);
    let (_, v) = node(&defaults, "AnsibleVariable", "default.web.list");
    assert!(v.properties.get("value_preview").is_none());

    let vars = extract("roles/web/vars/main.yml", "app_port: 80\n");
    let (_, v) = node(&vars, "AnsibleVariable", "var.web.app_port");
    assert_eq!(prop(v, "scope"), "var");

    let handlers = extract(
        "roles/web/handlers/main.yml",
        "- name: Reload nginx\n  systemd:\n    name: nginx\n    state: reloaded\n",
    );
    let (h, _) = node(&handlers, "AnsibleHandler", "handler.reload_nginx");
    symbol(&handlers, "DEPLOYS", h, "nginx");
}

#[test]
fn ansible_requirements_standalone_roles_and_plain_yaml() {
    let req = extract(
        "requirements.yml",
        "roles:\n  - name: geerlingguy.docker\n  - src: https://github.com/org/role.git\n",
    );
    let (_, r) = node(&req, "Role", "role.geerlingguy.docker");
    assert_eq!(prop(r, "confidence"), "medium");
    let (_, r) = node(&req, "Role", "role.https://github.com/org/role.git");
    assert_eq!(prop(r, "source"), "https://github.com/org/role.git");
    symbol(&req, "CALLS", 0, "https://github.com/org/role.git");
    symbol(&req, "CALLS", 0, "github.com");

    // A standalone role (the repository is the role): it is named after the repository.
    let standalone = extract("tasks/main.yml", "- name: Ping\n  ping:\n");
    let (t, _) = node(&standalone, "AnsibleTask", "role.repo.task.ping");
    assert_eq!(
        p(local(&standalone, "DEFINES", 0, t), "source_qualified_name"),
        "role.repo"
    );

    // A task list outside any role is a task file.
    let list = extract("playbooks/common.yml", "- name: Ping\n  ping:\n");
    node(&list, "AnsibleTask", "taskfile.common.task.ping");

    let plain = extract("config.yaml", "a: 1\nb: [2, 3]\n");
    assert_eq!(plain.nodes.len(), 1, "non-Ansible YAML: its file node");
    assert!(plain.edges.is_empty() && plain.incomplete.is_empty());

    let dup = extract("roles/x/tasks/main.yml", "- name: a\n  name: b\n");
    assert_eq!(dup.nodes.len(), 1, "a duplicate key fails the file");
    assert!(dup.incomplete[0].contains("Duplicate key"));
}

// ---------------------------------------------------------------------------------------------
// ColdFusion

const USER_CFC: &str = r#"<cfcomponent name="UserService" extends="BaseService" hint="users">
<cffunction name="getUser" access="public" returntype="struct">
  <cfquery name="qUser" datasource="db">
    SELECT * FROM Users u JOIN dbo.Orgs o ON o.id = u.org_id WHERE u.id = 1
  </cfquery>
  <cfinvoke component="lib.Mailer" method="send">
  <cfhttp url="https://api.example.com/users" method="post">
  <cfinclude template="/includes/header.cfm">
  <cfset var cache = createObject("component", "lib.Cache")>
  <cfreturn {}>
</cffunction>
<cffunction name="helper" returntype="string" access="private">
</cffunction>
</cfcomponent>
"#;

#[test]
fn coldfusion_tag_component() {
    let x = extract("app/User.cfc", USER_CFC);
    let (c, cn) = node(&x, "Component", "UserService");
    assert_eq!(cn.name, "UserService", "the name attribute, not the file");
    assert_eq!(prop(cn, "confidence_band"), "medium");
    assert_eq!(prop(cn, "language"), "coldfusion");
    let e = symbol(&x, "INHERITS", c, "BaseService");
    assert_eq!(p(e, "confidence_band"), "medium");

    let (g, f) = node(&x, "Function", "UserService.getUser");
    assert_eq!(f.parent, Some(c));
    assert_eq!(f.span.start_line, 2);
    assert_eq!(prop(f, "access"), "public");
    assert_eq!(prop(f, "return_type"), "struct");
    assert!(f.properties.get("syntax").is_none());
    let d = local(&x, "DEFINES_METHOD", c, g);
    assert_eq!(d.resolution, Resolution::Resolved);
    // The retained expression reads access, then returntype: a returntype before access is
    // missed.
    let (_, h) = node(&x, "Function", "UserService.helper");
    assert_eq!(prop(h, "access"), "private");
    assert_eq!(prop(h, "return_type"), "any");

    for t in ["Users", "Orgs"] {
        let e = symbol(&x, "QUERIES", c, t);
        assert_eq!(p(e, "confidence_band"), "low");
        assert_eq!(p(e, "query_name"), "qUser");
        assert_eq!(p(e, "source"), "cfquery");
    }
    let invokes = edges(
        &x,
        "CALLS",
        c,
        &EdgeTarget::Symbol("lib.Mailer.send".into()),
    );
    assert_eq!(invokes.len(), 2, "the pending edge and the unresolved call");
    assert!(invokes.iter().any(|e| p(e, "confidence_band") == "medium"));
    assert!(
        invokes
            .iter()
            .any(|e| p(e, "receiver_type") == "lib.Mailer" && (e.confidence - 0.6).abs() < 1e-6)
    );

    let route_qn = "UserService:HTTP:POST:https://api.example.com/users";
    let (route, r) = node(&x, "Route", route_qn);
    assert_eq!(r.name, "POST https://api.example.com/users");
    assert_eq!(prop(r, "http_method"), "POST");
    assert_eq!(prop(r, "url"), "https://api.example.com/users");
    assert_eq!(prop(r, "direction"), "outbound");
    let e = local(&x, "HTTP_CALLS", c, route);
    assert_eq!(p(e, "http_method"), "POST");

    let e = symbol(&x, "IMPORTS", c, "includes.header.cfm");
    assert_eq!(p(e, "template_path"), "/includes/header.cfm");
    symbol(&x, "USES_TYPE", c, "lib.Cache");
}

#[test]
fn coldfusion_script_component_and_template() {
    let x = extract(
        "app/OrderService.cfc",
        "component extends=\"BaseService\" implements=\"IOrders\" {\n    property name=\"repo\";\n    public struct function load(required numeric id) {\n        return helper(id);\n    }\n    private function helper(id) { return id; }\n    remote string function ping() { return \"ok\"; }\n}\n",
    );
    // `implements` defeats the retained component expression: the file name, no INHERITS; the
    // retained parser has no IMPLEMENTS, properties or calls between functions.
    let (c, _) = node(&x, "Component", "OrderService");
    assert!(
        x.edges
            .iter()
            .all(|e| e.kind != "INHERITS" && e.kind != "IMPLEMENTS")
    );
    let (l, f) = node(&x, "Function", "OrderService.load");
    assert_eq!(prop(f, "access"), "public");
    assert_eq!(prop(f, "return_type"), "struct");
    assert_eq!(prop(f, "syntax"), "cfscript");
    local(&x, "DEFINES_METHOD", c, l);
    let (_, f) = node(&x, "Function", "OrderService.helper");
    assert_eq!(
        (prop(f, "access"), prop(f, "return_type")),
        (&json!("private"), &json!("any"))
    );
    let (_, f) = node(&x, "Function", "OrderService.ping");
    assert_eq!(
        (prop(f, "access"), prop(f, "return_type")),
        (&json!("remote"), &json!("string"))
    );
    no_node(&x, "OrderService.repo");
    assert!(x.edges.iter().all(|e| e.kind != "CALLS"));

    let s = extract(
        "app/Svc.cfc",
        "component extends=\"Base\" {\n  function run() {}\n}\n",
    );
    let (c, _) = node(&s, "Component", "Svc");
    symbol(&s, "INHERITS", c, "Base");
    node(&s, "Function", "Svc.run");

    let t = extract(
        "views/index.cfm",
        "<cfinclude template=\"header.cfm\">\n<cfquery name=\"q\">UPDATE Orders SET x = 1</cfquery>\n<cffunction name=\"util\"></cffunction>\n",
    );
    let (_, f) = node(&t, "Function", "util");
    assert_eq!(f.parent, Some(0));
    assert!(t.edges.iter().all(|e| e.kind != "DEFINES_METHOD"));
    let e = symbol(&t, "IMPORTS", 0, "header.cfm");
    assert!(
        e.properties.get("source_qualified_name").is_none(),
        "the file's own qualified name is the file node"
    );
    symbol(&t, "QUERIES", 0, "Orders");
    assert!(
        t.nodes.iter().all(|n| n.kind != "Component"),
        "a template has no component"
    );
}
