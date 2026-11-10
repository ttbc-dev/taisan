import copy
import importlib.util
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / 'scripts' / 's1' / 'check-prerequisites.py'
spec = importlib.util.spec_from_file_location('s1_prerequisites', MODULE_PATH)
prerequisites = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prerequisites)

FIXTURE_DIR = ROOT / 'fixtures' / 's1'


class S1MaterialsTest(unittest.TestCase):
    def setUp(self):
        self.install_spec = json.loads((FIXTURE_DIR / 'installation-spec.json').read_text())
        self.seeded_issue = json.loads((FIXTURE_DIR / 'seeded-issue.json').read_text())
        self.checklist = json.loads((FIXTURE_DIR / 'evidence-checklist.json').read_text())

    def errors_after(self, mutate):
        spec = copy.deepcopy(self.install_spec)
        mutate(spec)
        return prerequisites.validate_install_spec(spec)

    def test_all_three_material_fixtures_are_present(self):
        for relative in prerequisites.FIXTURE_FILES.values():
            self.assertTrue((ROOT / relative).is_file(), relative)
        report = prerequisites.verify(ROOT)
        self.assertEqual(report['status'], 'ok', report['errors'])
        self.assertEqual(report['errors'], [])

    def test_complete_install_spec_reports_success_without_live_execution(self):
        report = prerequisites.verify(ROOT)
        self.assertEqual(report['status'], 'ok', report['errors'])
        self.assertEqual(report['errors'], [])
        self.assertIn('not-run', report['live_execution'])
        result = subprocess.run(
            [sys.executable, str(MODULE_PATH), '--root', str(ROOT)],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        output = json.loads(result.stdout)
        self.assertEqual(output['status'], 'ok')
        self.assertIn('not-run', output['live_execution'])

    def test_all_three_variants_are_defined(self):
        variants = set(prerequisites.VARIANTS)
        self.assertEqual(set(self.seeded_issue['variants']), variants)
        self.assertEqual(set(self.checklist['variants']), variants)
        readme = (ROOT / 'docs' / 's1' / 'README.md').read_text().lower()
        checklist = (ROOT / 'docs' / 's1' / 'evidence-checklist.md').read_text().lower()
        for variant in variants:
            self.assertIn(variant.lower(), readme)
            self.assertIn(variant.lower(), checklist)
        self.assertEqual(prerequisites.validate_seeded_issue(self.seeded_issue), [])
        self.assertEqual(prerequisites.validate_checklist(self.checklist), [])

    def test_every_s1_criterion_row_has_a_variant_assignment(self):
        documented = prerequisites._markdown_row_ids(
            ROOT / 'docs' / 's1' / 'evidence-checklist.md'
        )
        self.assertTrue(documented)
        rows = {row['id']: row for row in self.checklist['rows']}
        self.assertFalse(documented - set(rows), documented - set(rows))
        for row in self.checklist['rows']:
            self.assertIn(row['variant'], prerequisites.VARIANTS, row['id'])
        self.assertEqual(prerequisites.validate_markdown(ROOT, self.checklist), [])

    def test_missing_allowlist_fails_closed(self):
        errors = self.errors_after(lambda spec: spec.pop('installation_allowlist'))
        self.assertTrue(any('installation_allowlist' in error for error in errors), errors)

    def test_empty_allowlist_fails_closed(self):
        errors = self.errors_after(lambda spec: spec.__setitem__('installation_allowlist', []))
        self.assertTrue(any('installation_allowlist' in error for error in errors), errors)

    def test_allowlist_extra_repository_fails_closed(self):
        errors = self.errors_after(
            lambda spec: spec['installation_allowlist'].append('shimpeiws/another-repo')
        )
        self.assertTrue(any('installation_allowlist' in error for error in errors), errors)

    def test_missing_credential_reference_fails_closed(self):
        errors = self.errors_after(lambda spec: spec.pop('credential_reference'))
        self.assertTrue(any('credential_reference' in error for error in errors), errors)

    def test_empty_credential_reference_fails_closed(self):
        errors = self.errors_after(lambda spec: spec.__setitem__('credential_reference', {}))
        self.assertTrue(any('credential_reference' in error for error in errors), errors)

    def test_credential_value_fails_closed(self):
        def add_value(spec):
            spec['credential_reference']['value'] = 'ghp_not-a-real-secret'

        errors = self.errors_after(add_value)
        self.assertTrue(any('never carry a value' in error for error in errors), errors)

    def test_missing_reviewer_identity_fails_closed(self):
        errors = self.errors_after(lambda spec: spec.pop('reviewer_identity'))
        self.assertTrue(any('reviewer_identity' in error for error in errors), errors)

    def test_reviewer_missing_app_identity_fails_closed(self):
        errors = self.errors_after(
            lambda spec: spec['reviewer_identity'].pop('app_id')
        )
        self.assertTrue(any('reviewer_identity.app_id' in error for error in errors), errors)

    def test_reviewer_scope_must_be_the_isolated_repository(self):
        errors = self.errors_after(
            lambda spec: spec['reviewer_identity'].__setitem__(
                'installation_scope', ['shimpeiws/coding-worker-smoke', 'shimpeiws/other']
            )
        )
        self.assertTrue(
            any('reviewer_identity.installation_scope' in error for error in errors),
            errors,
        )

    def test_missing_worker_runtime_identity_fails_closed(self):
        errors = self.errors_after(lambda spec: spec.pop('worker_runtime_identity'))
        self.assertTrue(any('worker_runtime_identity' in error for error in errors), errors)

    def test_missing_revisions_or_configuration_fails_closed(self):
        missing_revisions = self.errors_after(lambda spec: spec.pop('revisions'))
        self.assertTrue(any('revisions' in error for error in missing_revisions))
        missing_config = self.errors_after(lambda spec: spec.pop('configuration_identity'))
        self.assertTrue(any('configuration_identity' in error for error in missing_config))

    def test_every_checklist_entry_has_revision_config_and_identity_fields(self):
        for row in self.checklist['rows']:
            self.assertIsInstance(row['revision_config'], dict, row['id'])
            for key in prerequisites.REVISION_KEYS:
                self.assertTrue(row['revision_config'].get(key), f"{row['id']} {key}")
            for key in prerequisites.IDENTITY_KEYS:
                self.assertIn(key, row['identities'], row['id'])
                self.assertIsInstance(row['identities'][key], list, row['id'])

        broken = copy.deepcopy(self.checklist)
        broken['rows'][0]['identities'].pop('action')
        errors = prerequisites.validate_checklist(broken)
        self.assertTrue(any('identities.action' in error for error in errors), errors)

    def test_missing_material_file_fails_closed(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            shutil.copytree(FIXTURE_DIR, root / 'fixtures' / 's1')
            shutil.copytree(ROOT / 'docs' / 's1', root / 'docs' / 's1')
            (root / 'fixtures' / 's1' / 'seeded-issue.json').unlink()
            report = prerequisites.verify(root)
            self.assertEqual(report['status'], 'failed')
            self.assertTrue(
                any('seeded-issue: fixture file missing' in error for error in report['errors']),
                report['errors'],
            )

    def test_malformed_install_spec_json_fails_closed(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / 'install.json'
            path.write_text('{ not json')
            result = subprocess.run(
                [sys.executable, str(MODULE_PATH), '--root', str(ROOT), '--install-spec', str(path)],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 1)
            output = json.loads(result.stdout)
            self.assertEqual(output['status'], 'failed')
            self.assertTrue(
                any('malformed JSON' in error for error in output['errors']),
                output['errors'],
            )

    def test_duplicate_checklist_row_id_fails_closed(self):
        broken = copy.deepcopy(self.checklist)
        broken['rows'].append(copy.deepcopy(broken['rows'][0]))
        errors = prerequisites.validate_checklist(broken)
        self.assertTrue(any('duplicate' in error for error in errors), errors)

    def test_missing_variant_fails_closed(self):
        broken = copy.deepcopy(self.checklist)
        broken['variants'].pop('Attention')
        errors = prerequisites.validate_checklist(broken)
        self.assertTrue(any('variants' in error for error in errors), errors)

    def test_seeded_issue_with_an_automation_label_fails_closed(self):
        broken = copy.deepcopy(self.seeded_issue)
        broken['labels'] = ['automation-lane-selected']
        errors = prerequisites.validate_seeded_issue(broken)
        self.assertTrue(any('labels' in error for error in errors), errors)

    def test_cli_fails_closed_on_incomplete_install_spec(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / 'install.json'
            broken = copy.deepcopy(self.install_spec)
            broken.pop('reviewer_identity')
            path.write_text(json.dumps(broken))
            result = subprocess.run(
                [sys.executable, str(MODULE_PATH), '--root', str(ROOT), '--install-spec', str(path)],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 1)
            output = json.loads(result.stdout)
            self.assertEqual(output['status'], 'failed')
            self.assertTrue(
                any('reviewer_identity' in error for error in output['errors']),
                output['errors'],
            )


if __name__ == '__main__':
    unittest.main()
