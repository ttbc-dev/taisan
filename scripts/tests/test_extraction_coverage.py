import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

MODULE_PATH = Path(__file__).resolve().parents[1] / 'check-extraction-coverage.py'
spec = importlib.util.spec_from_file_location('extraction_coverage', MODULE_PATH)
coverage = importlib.util.module_from_spec(spec)
spec.loader.exec_module(coverage)


class CoverageCheckTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'spec.md').write_text('# Contract\n')
        sources, files, private_files = [], [], []
        for role in sorted(coverage.ROLES):
            key = role + '-F1'
            sources.append({'role': role, 'revision': 'a' * 40, 'file_ids': [key], 'file_count': 1})
            files.append({'id': key, 'role': role, 'blob': 'b' * 40,
                          'status': 'reviewed', 'rationale': 'Reviewed entrypoint and its safety test.',
                          'audit_evidence': key, 'behavior_ids': ['B1']})
            private_files.append({'id': key, 'role': role, 'blob': 'b' * 40,
                                  'path': 'source-file', 'mode': '100644', 'kind': 'blob',
                                  'review_evidence': {'method': 'definition/test review', 'regions': ['entrypoint']}})
        behavior = {'id': 'B1', 'disposition': 'retain', 'rationale': 'Preserve stop safety.',
                    'requirement_ids': ['R01']}
        detail = {'id': 'B1'}
        for key in ('trigger', 'preconditions', 'inputs_outputs', 'owners', 'durable_state',
                    'identities', 'transitions', 'failure_semantics', 'concurrency_restart',
                    'bounds', 'human_action', 'source_evidence', 'evidence_classification'):
            detail[key] = 'Explicit fixture contract.'
        requirements = [
            {'id': f'R{i:02}', 'status': 'missing', 'implementation_units': ['U1'],
             'planned_proof': ['Cancel while a child survives; ownership stays held.'],
             'targets': [{'document': 'spec.md', 'anchor': 'contract', 'owner_issue': 50,
                          'acceptance_id': 'I50-AC1', 'state': 'specified'}]}
            for i in range(1, 39)
        ]
        self.document = {'schema_version': 1, 'sources': sources, 'files': files,
                         'behaviors': [behavior], 'requirements': requirements}
        self.catalog = {'sources': copy.deepcopy(sources), 'files': private_files,
                        'behaviors': [detail], 'acceptance': [{'id': 'I50-AC1', 'issue': 50}]}

    def result(self):
        return coverage.check(self.document, self.catalog, self.root)

    def codes(self, kind='errors'):
        return {item['code'] for item in self.result()[kind]}

    def run_cli(self, *args):
        docs = self.root / 'docs'
        docs.mkdir(exist_ok=True)
        (docs / 'extraction-coverage.json').write_text(json.dumps(self.document))
        block = coverage.AUDIT_START + '\n```json\n' + json.dumps(self.catalog) + '\n```\n' + coverage.AUDIT_END
        (docs / 'source-workflow-audit.md').write_text(block)
        result = subprocess.run([sys.executable, str(MODULE_PATH), '--root', str(self.root), *args],
                                capture_output=True, text=True, check=False)
        return result.returncode, json.loads(result.stdout)

    def test_cli_pending_design_option_preserves_incomplete_result(self):
        self.document['requirements'][0]['targets'][0]['state'] = 'planned'
        self.assertEqual(self.run_cli()[0], 1)
        code, output = self.run_cli('--allow-pending-design')
        self.assertEqual(code, 0)
        self.assertTrue(output['mapping_valid'])
        self.assertFalse(output['complete'])
        self.assertEqual([x['code'] for x in output['gaps']], ['DESIGN_PENDING'])

    def test_cli_pending_design_option_never_allows_unreviewed_source(self):
        self.document['files'][0]['status'] = 'pending'
        code, output = self.run_cli('--allow-pending-design')
        self.assertEqual(code, 1)
        self.assertFalse(output['complete'])
        self.assertEqual([x['code'] for x in output['gaps']], ['FILE_UNREVIEWED'])

    def test_complete_mapping_does_not_claim_runtime_correctness(self):
        result = self.result()
        self.assertTrue(result['complete'])
        self.assertEqual(result['requirement_count'], 38)
        self.assertIn('runtime correctness require independent evidence', result['scope'])

    def test_pending_file_blocks_completeness(self):
        self.document['files'][0]['status'] = 'pending'
        self.assertFalse(self.result()['complete'])
        self.assertEqual(self.codes('gaps'), {'FILE_UNREVIEWED'})

    def test_file_omitted_from_public_manifest_is_detected(self):
        self.document['files'].pop()
        self.assertIn('FILE_SET', self.codes())
        self.assertIn('ROLE_FILE_SET', self.codes())
        self.assertIn('FILE_COUNT', self.codes())

    def test_review_claim_needs_actual_owner_record(self):
        self.catalog['files'][0].pop('review_evidence')
        self.assertIn('REVIEW_EVIDENCE_MISSING', self.codes())

    def test_unclassified_behavior_and_unmapped_requirement_fail(self):
        self.document['behaviors'][0]['disposition'] = ''
        self.document['behaviors'][0]['requirement_ids'] = ['R99']
        self.assertIn('UNCLASSIFIED_BEHAVIOR', self.codes())
        self.assertIn('REQUIREMENT_LINK_MISSING', self.codes())

    def test_omission_still_needs_requirement_and_source_mapping(self):
        self.document['behaviors'][0]['disposition'] = 'omit'
        self.document['behaviors'][0]['requirement_ids'] = []
        for entry in self.document['files']:
            entry['behavior_ids'] = []
        self.assertIn('REQUIREMENT_LINK_MISSING', self.codes())
        self.assertIn('BEHAVIOR_SOURCE_LINK_MISSING', self.codes())

    def test_target_owner_acceptance_and_anchor_are_checked(self):
        target = self.document['requirements'][0]['targets'][0]
        target['acceptance_id'] = 'I51-AC9'
        target['anchor'] = 'missing-heading'
        self.assertIn('AC_LINK_MISSING', self.codes())
        self.assertIn('SPECIFICATION_ANCHOR', self.codes())

    def test_legacy_requirement_cannot_disappear(self):
        self.document['requirements'].pop()
        self.assertIn('LEGACY_REQUIREMENT_MISSING', self.codes())

    def test_stale_source_pin_and_blob_are_detected(self):
        self.catalog['sources'][0]['revision'] = 'c' * 40
        self.catalog['files'][0]['blob'] = 'd' * 40
        self.assertIn('PIN_MISMATCH', self.codes())
        self.assertIn('BLOB_MISMATCH', self.codes())

    def test_owner_catalog_cannot_duplicate_acceptance_or_omit_source(self):
        self.catalog['acceptance'].append(copy.deepcopy(self.catalog['acceptance'][0]))
        self.catalog['sources'].pop()
        self.assertIn('DUPLICATE_ID', self.codes())
        self.assertIn('AUDIT_SOURCE_SET', self.codes())

    def test_catalog_file_role_and_blob_identity_are_checked(self):
        self.catalog['files'][0]['role'] = 'other'
        self.document['files'][0]['blob'] = ''
        self.assertIn('AUDIT_FILE_ROLE', self.codes())
        self.assertIn('BLOB_ID', self.codes())

    def test_deferral_without_owner_disposition_remains_a_gap(self):
        self.document['behaviors'][0]['disposition'] = 'defer'
        self.assertEqual(self.codes('gaps'), {'OWNER_DISPOSITION_PENDING'})

    def test_future_design_is_reported_not_counted_complete(self):
        self.document['requirements'][0]['targets'][0]['state'] = 'planned'
        result = self.result()
        self.assertTrue(result['mapping_valid'])
        self.assertFalse(result['complete'])
        self.assertEqual(self.codes('gaps'), {'DESIGN_PENDING'})

    def test_private_source_locator_rejected_from_public_file_entry(self):
        self.document['files'][0]['path'] = '/private/source/entrypoint'
        self.assertIn('PRIVATE_LOCATOR', self.codes())

    def test_target_path_cannot_escape_repository(self):
        self.document['requirements'][0]['targets'][0]['document'] = '../outside.md'
        self.assertIn('SPECIFICATION_PATH', self.codes())

    def test_catalog_requires_exactly_one_fenced_data_block(self):
        block = coverage.AUDIT_START + '\n```json\n{}\n```\n' + coverage.AUDIT_END
        self.assertEqual(coverage.audit_catalog(block), {})
        with self.assertRaisesRegex(ValueError, 'one extraction catalog'):
            coverage.audit_catalog(block + block)

    def test_malformed_sections_are_rejected_at_boundary(self):
        self.document['files'] = 'not an array'
        with self.assertRaisesRegex(ValueError, 'array of objects'):
            self.result()


if __name__ == '__main__':
    unittest.main()
