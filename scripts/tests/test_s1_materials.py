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

    def complete_install_spec(self):
        spec = copy.deepcopy(self.install_spec)
        spec['repository']['stable_id'] = 'R_fixture_s1_repository'
        spec['reviewer_identity']['app_id'] = '1234567'
        spec['configuration_identity']['store_id'] = 'store-s1-fixture'
        target = spec['implementation_status']['target_profile']
        target['adapter_available'] = True
        target['reviewer_app_available'] = True
        return spec

    def test_all_three_material_fixtures_are_present(self):
        for relative in prerequisites.FIXTURE_FILES.values():
            self.assertTrue((ROOT / relative).is_file(), relative)
        report = prerequisites.verify(ROOT)
        self.assertEqual(report['errors'], [])
        self.assertEqual(report['status'], 'prepared', report['errors'])

    def test_shipped_spec_is_prepared_not_certified_as_installed(self):
        report = prerequisites.verify(ROOT)
        self.assertEqual(report['status'], 'prepared', report['errors'])
        self.assertFalse(report['installation_ready'])
        unresolved = report['unresolved_identities']
        self.assertTrue(unresolved)
        for field in (
            'repository.stable_id',
            'reviewer_identity.app_id',
            'configuration_identity.store_id',
        ):
            self.assertTrue(
                any(field in item for item in unresolved), (field, unresolved)
            )
        self.assertTrue(any('adapter' in item for item in unresolved), unresolved)
        self.assertTrue(
            any('reviewer app' in item.lower() for item in unresolved), unresolved
        )

    def test_complete_install_spec_reports_success_without_live_execution(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / 'install.json'
            path.write_text(json.dumps(self.complete_install_spec()))
            report = prerequisites.verify(ROOT, path)
            self.assertEqual(report['status'], 'ok', report['errors'])
            self.assertEqual(report['errors'], [])
            self.assertTrue(report['installation_ready'])
            self.assertEqual(report['unresolved_identities'], [])
            self.assertIn('not-run', report['live_execution'])
            result = subprocess.run(
                [
                    sys.executable,
                    str(MODULE_PATH),
                    '--root',
                    str(ROOT),
                    '--install-spec',
                    str(path),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            output = json.loads(result.stdout)
            self.assertEqual(output['status'], 'ok')
            self.assertTrue(output['installation_ready'])
            self.assertEqual(output['unresolved_identities'], [])
            self.assertIn('not-run', output['live_execution'])

    def test_placeholder_binding_identities_fail_closed(self):
        spec = copy.deepcopy(self.install_spec)
        spec['repository']['stable_id'] = 'recorded-from-host-read-at-preflight'
        spec['reviewer_identity']['app_id'] = (
            'recorded-from-owner-app-listing-at-preflight'
        )
        spec['configuration_identity']['store_id'] = 'recorded-at-preflight'
        errors = prerequisites.validate_install_spec(spec)
        for field in (
            'repository.stable_id',
            'reviewer_identity.app_id',
            'configuration_identity.store_id',
        ):
            self.assertTrue(any(field in error for error in errors), (field, errors))

    def test_missing_required_capability_fails_closed(self):
        for capability in (
            'push_ref',
            'change_request_create',
            'change_request_update',
            'comment_write',
        ):
            spec = copy.deepcopy(self.install_spec)
            spec['configuration_identity']['capabilities'].remove(capability)
            errors = prerequisites.validate_install_spec(spec)
            self.assertTrue(
                any('capabilities' in error and capability in error for error in errors),
                (capability, errors),
            )

    def test_missing_required_check_fails_closed(self):
        for required_checks in ([], ['not-verify'], ['verify', 'extra']):
            spec = copy.deepcopy(self.install_spec)
            spec['configuration_identity']['required_checks'] = required_checks
            errors = prerequisites.validate_install_spec(spec)
            self.assertTrue(any('required_checks' in error for error in errors), errors)

    def test_non_positive_budget_fails_closed(self):
        for key in ('max_invocations', 'max_review_rounds', 'runtime_limit_seconds'):
            for value in (0, -1, '4', None):
                spec = copy.deepcopy(self.install_spec)
                spec['configuration_identity']['budget'][key] = value
                errors = prerequisites.validate_install_spec(spec)
                self.assertTrue(
                    any('budget' in error and key in error for error in errors),
                    (key, value, errors),
                )

    def test_missing_budget_field_fails_closed(self):
        spec = copy.deepcopy(self.install_spec)
        spec['configuration_identity']['budget'].pop('max_review_rounds')
        errors = prerequisites.validate_install_spec(spec)
        self.assertTrue(any('budget' in error for error in errors), errors)

    def test_retained_rehearsal_profile_matches_live_evidence(self):
        status = self.install_spec['implementation_status']
        target = status['target_profile']
        self.assertFalse(target['adapter_available'])
        self.assertFalse(target['reviewer_app_available'])
        rehearsals = status['retained_rehearsals']
        self.assertEqual(len(rehearsals), 2)
        for rehearsal in rehearsals:
            document = (ROOT / rehearsal['evidence']).read_text()
            self.assertIn(rehearsal['run_tag'], document)
            self.assertIn(rehearsal['adapter'], document)
            self.assertIn(rehearsal['reviewer_actor'], document)
            self.assertIn(rehearsal['credential_reference'], document)
            self.assertIn('host_write: true', document)
            self.assertTrue(rehearsal['repository_host_write'])
        self.assertEqual(
            prerequisites.validate_rehearsals(ROOT, self.install_spec), []
        )
        for key, value in (
            ('adapter', 'not-the-recorded-adapter'),
            ('credential_reference', 'defaults'),
            ('credential_reference', 'not-the-recorded-credential'),
            ('reviewer_actor', 'not-the-recorded-reviewer'),
        ):
            with self.subTest(key=key, value=value):
                broken = copy.deepcopy(self.install_spec)
                broken['implementation_status']['retained_rehearsals'][0][key] = value
                errors = prerequisites.validate_rehearsals(ROOT, broken)
                self.assertTrue(
                    any(key in error for error in errors), (key, value, errors)
                )

    def test_fixture_content_digest_is_computed_and_recorded(self):
        report = prerequisites.verify(ROOT)
        computed = prerequisites.fixture_content_digest(ROOT)
        self.assertEqual(report['fixture_content_digest'], computed)
        evidence = (ROOT / 'docs' / 'evidence' / 'u8.1.md').read_text()
        self.assertEqual(evidence.count(computed), 2, evidence[:200])
        self.assertEqual(report['errors'], [])

    def test_stale_fixture_content_digest_fails_closed(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            shutil.copytree(FIXTURE_DIR, root / 'fixtures' / 's1')
            shutil.copytree(ROOT / 'docs' / 's1', root / 'docs' / 's1')
            shutil.copytree(
                ROOT / 'docs' / 'evidence', root / 'docs' / 'evidence'
            )
            seed_path = root / 'fixtures' / 's1' / 'seeded-issue.json'
            seed = json.loads(seed_path.read_text())
            seed['body_template'] += ' '
            seed_path.write_text(json.dumps(seed, indent=2) + '\n')
            report = prerequisites.verify(root)
            self.assertEqual(report['status'], 'failed')
            self.assertTrue(
                any(
                    'fixture content digest' in error and 'does not match' in error
                    for error in report['errors']
                ),
                report['errors'],
            )

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
        expected = prerequisites.expected_revision_config(self.install_spec)
        revisions = self.install_spec['revisions']
        self.assertEqual(expected['taisan_core_revision'], revisions['taisan_core'])
        self.assertEqual(expected['taisan_cli_revision'], revisions['taisan_cli'])
        self.assertEqual(
            expected['github_adapter_revision'], revisions['github_adapter']
        )
        self.assertEqual(
            expected['worker_adapter_revision'], revisions['worker_adapter']
        )
        self.assertIn('config_sha256=', expected['configuration_identity'])
        for row in self.checklist['rows']:
            self.assertIsInstance(row['revision_config'], dict, row['id'])
            self.assertEqual(row['revision_config'], expected, row['id'])
            for key in prerequisites.SHA_REVISION_KEYS:
                self.assertRegex(
                    row['revision_config'][key], r'^[0-9a-f]{40}$', row['id']
                )
            for key in prerequisites.IDENTITY_KEYS:
                self.assertIn(key, row['identities'], row['id'])
                self.assertIsInstance(row['identities'][key], list, row['id'])
        self.assertEqual(prerequisites.validate_checklist(self.checklist), [])

        broken = copy.deepcopy(self.checklist)
        broken['rows'][0]['revision_config']['taisan_core_revision'] = (
            'exact SHA from installation-spec.revisions.taisan_core'
        )
        errors = prerequisites.validate_checklist(broken)
        self.assertTrue(
            any('taisan_core_revision' in error for error in errors), errors
        )
        broken = copy.deepcopy(self.checklist)
        broken['rows'][0]['revision_config']['configuration_identity'] = (
            'installation-spec.configuration_identity'
        )
        errors = prerequisites.validate_checklist(broken)
        self.assertTrue(
            any('configuration_identity' in error for error in errors), errors
        )
        broken = copy.deepcopy(self.checklist)
        broken['rows'][0]['revision_config']['taisan_core_revision'] = 'a' * 40
        errors = prerequisites.validate_checklist_revision_config(
            broken, self.install_spec
        )
        self.assertTrue(
            any('revision_config' in error for error in errors), errors
        )
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

    def test_evidence_and_revisions_cite_the_slice_commit_not_the_base(self):
        base = 'fb70d123fec2e011914af2c121c341f340965923'
        revisions = self.install_spec['revisions']
        slice_sha = revisions['materials_base_commit']
        self.assertNotEqual(slice_sha, base)
        self.assertRegex(slice_sha, r'^[0-9a-f]{40}$')
        for key in (
            'taisan_core',
            'taisan_cli',
            'github_adapter',
            'worker_adapter',
        ):
            self.assertEqual(revisions[key], slice_sha, key)
        evidence = (ROOT / 'docs' / 'evidence' / 'u8.1.md').read_text()
        self.assertIn('Slice revision', evidence)
        self.assertIn(slice_sha, evidence)
        self.assertNotIn('Slice revision | The tested tree is the uncommitted', evidence)

    def test_checklist_top_level_revision_config_is_exact(self):
        expected = prerequisites.expected_revision_config(self.install_spec)
        self.assertEqual(self.checklist['revision_config'], expected)
        broken = copy.deepcopy(self.checklist)
        broken['revision_config']['taisan_core_revision'] = (
            'exact SHA from installation-spec.revisions.taisan_core'
        )
        errors = prerequisites.validate_checklist(broken)
        self.assertTrue(
            any('revision_config.taisan_core_revision' in error for error in errors),
            errors,
        )
        broken = copy.deepcopy(self.checklist)
        broken['revision_config']['configuration_identity'] = (
            'installation-spec.configuration_identity'
        )
        errors = prerequisites.validate_checklist(broken)
        self.assertTrue(
            any(
                'revision_config.configuration_identity' in error
                for error in errors
            ),
            errors,
        )
        broken = copy.deepcopy(self.checklist)
        broken['revision_config']['taisan_core_revision'] = 'a' * 40
        errors = prerequisites.validate_checklist_revision_config(
            broken, self.install_spec
        )
        self.assertTrue(
            any(error.startswith('revision_config:') for error in errors), errors
        )

    def test_missing_evidence_digest_record_fails_closed(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            shutil.copytree(FIXTURE_DIR, root / 'fixtures' / 's1')
            shutil.copytree(ROOT / 'docs' / 's1', root / 'docs' / 's1')
            shutil.copytree(
                ROOT / 'docs' / 'evidence', root / 'docs' / 'evidence'
            )
            (root / 'docs' / 'evidence' / 'u8.1.md').unlink()
            report = prerequisites.verify(root)
            self.assertEqual(report['status'], 'failed')
            self.assertTrue(
                any(
                    'missing required fixture content digest record' in error
                    for error in report['errors']
                ),
                report['errors'],
            )

    def test_explicit_install_spec_still_validates_checklist_revisions(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            shutil.copytree(FIXTURE_DIR, root / 'fixtures' / 's1')
            shutil.copytree(ROOT / 'docs' / 's1', root / 'docs' / 's1')
            shutil.copytree(
                ROOT / 'docs' / 'evidence', root / 'docs' / 'evidence'
            )
            checklist_path = root / 'fixtures' / 's1' / 'evidence-checklist.json'
            checklist = json.loads(checklist_path.read_text())
            checklist['rows'][0]['revision_config']['taisan_core_revision'] = 'a' * 40
            checklist_path.write_text(json.dumps(checklist, indent=2) + '\n')
            shipped = root / 'fixtures' / 's1' / 'installation-spec.json'
            report = prerequisites.verify(root, shipped)
            self.assertEqual(report['status'], 'failed')
            self.assertTrue(
                any('revision_config' in error for error in report['errors']),
                report['errors'],
            )
            result = subprocess.run(
                [
                    sys.executable,
                    str(MODULE_PATH),
                    '--root',
                    str(root),
                    '--install-spec',
                    str(shipped),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 1, result.stderr)
            output = json.loads(result.stdout)
            self.assertTrue(
                any('revision_config' in error for error in output['errors']),
                output['errors'],
            )

    def test_malformed_but_json_valid_types_fail_closed(self):
        seed = copy.deepcopy(self.seeded_issue)
        seed['title_template'] = 123
        errors = prerequisites.validate_seeded_issue(seed)
        self.assertTrue(any('title_template' in error for error in errors), errors)

        seed = copy.deepcopy(self.seeded_issue)
        seed['variants']['success'] = 'not-an-object'
        errors = prerequisites.validate_seeded_issue(seed)
        self.assertTrue(any('variants.success' in error for error in errors), errors)

        spec = copy.deepcopy(self.install_spec)
        spec['configuration_identity']['capabilities'].append({'name': 'bad'})
        errors = prerequisites.validate_install_spec(spec)
        self.assertTrue(any('capabilities' in error for error in errors), errors)

        broken = copy.deepcopy(self.checklist)
        broken['rows'][0]['id'] = ['not', 'a', 'string']
        errors = prerequisites.validate_checklist(broken)
        self.assertTrue(any('.id' in error for error in errors), errors)

        broken = copy.deepcopy(self.checklist)
        broken['rows'] = None
        errors = prerequisites.validate_checklist(broken)
        self.assertTrue(any('rows' in error for error in errors), errors)

    def test_cli_fails_closed_on_malformed_input_types(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / 'install.json'
            broken = copy.deepcopy(self.install_spec)
            broken['configuration_identity']['capabilities'].append({'name': 'bad'})
            path.write_text(json.dumps(broken))
            result = subprocess.run(
                [
                    sys.executable,
                    str(MODULE_PATH),
                    '--root',
                    str(ROOT),
                    '--install-spec',
                    str(path),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 1, result.stderr)
            output = json.loads(result.stdout)
            self.assertEqual(output['status'], 'failed')
            self.assertTrue(
                any('capabilities' in error for error in output['errors']),
                output['errors'],
            )

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
