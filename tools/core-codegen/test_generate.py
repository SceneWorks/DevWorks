"""Checks of the core code generator's schema-revision rules (standard library only).

Run from the repository root: python3 -m unittest discover -s tools/core-codegen -p 'test_*.py'
"""
import copy
import importlib.util
import json
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
_SPEC = importlib.util.spec_from_file_location('generate', HERE / 'generate.py')
generate = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(generate)


class SchemaRevisionRules(unittest.TestCase):
    def setUp(self):
        self.types = copy.deepcopy(generate.TYPES)
        self.revisions = copy.deepcopy(generate.REVISIONS)

    def tearDown(self):
        generate.TYPES.clear()
        generate.TYPES.update(self.types)
        generate.REVISIONS.clear()
        generate.REVISIONS.update(self.revisions)
        generate.RC = generate.revisioned_types()

    def test_the_recorded_spec_and_register_generate(self):
        names = {o['operation'] for _, o in generate.operations()}
        self.assertIn('artifacts.save', names)
        carried = {o['operation']: generate.introduced_in(o) for _, o in generate.operations()}
        self.assertEqual({n for n, s in carried.items() if s}, {
            'artifacts.save', 'artifacts.list', 'history.events', 'history.work_lineage',
            'history.git_links', 'history.evidence_links', 'history.tool_usage', 'sessions.briefing',
            'resources.list', 'resources.acquire', 'resources.renew', 'resources.release',
            'resources.holders', 'resources.waiters'})
        self.assertEqual(carried['resources.renew'], {2})
        self.assertEqual(carried['artifacts.save'], {2, 3})
        # Shared envelopes and union alternatives count only for the operation that selects them:
        # runs.inspect shares the query envelope and reaches Subject (via blockers), never the
        # command aggregate or project scope of history.events.
        self.assertEqual(carried['history.events'], {2})
        self.assertEqual(carried['runs.inspect'], set())
        self.assertEqual(carried['history.operation_status'], set())

    def test_a_history_page_result_is_projected(self):
        # HistoryPage gates its own checkpoint, so every result carrying it is projected.
        results = generate.gen_results(generate.operations())
        for op in ['HistoryEvents', 'HistoryWorkLineage', 'HistoryGitLinks', 'HistoryEvidenceLinks', 'HistoryToolUsage']:
            self.assertIn(f'Self::{op} {{ value }} => value.retain_revision(revision),', results)

    def test_a_gated_member_of_an_operation_that_never_registers_its_revision_fails(self):
        # ProjectRecord is the result of projects.get, which registers only revision 1.
        record = generate.TYPES['ProjectRecord']
        record['fields']['labels'] = '?Label'
        record['since'] = {'labels': 2}
        generate.RC = generate.revisioned_types()
        with self.assertRaises(SystemExit) as failure:
            generate.operations()
        self.assertIn('introduced at schema revision 2', str(failure.exception))
        self.assertIn('projects.', str(failure.exception))

    def test_registering_the_revision_makes_the_member_reachable(self):
        self.assertTrue(generate.introduced_in(
            next(o for _, o in generate.operations() if o['operation'] == 'artifacts.list')))
        generate.REVISIONS['artifacts.list'] = [1]
        with self.assertRaises(SystemExit):
            generate.operations()

    def test_a_shared_envelope_counts_only_the_branch_its_selector_picks(self):
        doc = json.loads((generate.CONTRACTS / 'artifact-command-request.r2.schema.json').read_text())
        self.assertEqual(generate.doc_since(doc, '/command/kind', 'Save'), {2})
        # The revision-3 Save branch: tags (2) and the workspace scope alternative (3), whose
        # own oneOf belongs to the branch rather than making it a union.
        doc3 = json.loads((generate.CONTRACTS / 'artifact-command-request.r3.schema.json').read_text())
        self.assertEqual(generate.doc_since(doc3, '/command/kind', 'Save'), {2, 3})
        self.assertEqual(generate.doc_since(doc3, '/command/kind', 'Reference'), set())
        self.assertEqual(generate.doc_since(doc, '/command/kind', 'Reference'), set())
        self.assertEqual(generate.doc_since(doc), {2})

    def test_an_exactly_one_choice_lists_each_alternatives_full_required_set(self):
        # The revision-1 alternative is the recorded required list, so an earlier revision
        # document folds back to exactly that list; later members carry their x-since.
        op = generate.DOMAIN_OPS['history.tool_usage']
        attr = generate.choice(op['fields'], op['exactlyOne'], op['since'])
        self.assertIn('{"required": ["rootId", "page"]}', attr)
        self.assertIn('{"required": ["projectId", "page"], "x-since": 2}', attr)
        self.assertIn('{"required": ["workspace", "page"], "x-since": 2}', attr)
        record = generate.record_attrs(generate.TYPES['AttributedGitLink'])
        self.assertIn('"x-earlier" = {"name": "GitLink", "before": 2}', record)
        self.assertIn('rename = "Page"', generate.record_attrs(generate.TYPES['UsagePage']))


if __name__ == '__main__':
    unittest.main()
