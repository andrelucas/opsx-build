import contextlib
import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import preflight


class GoplsCheckTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='opsx-preflight-test-')
        self.addCleanup(temporary.cleanup)
        self.scratch = Path(temporary.name)
        self.cache = self.scratch / 'cache'
        self.source = self.scratch / 'gopls/main.go'
        self.diagnostic = (
            f'{self.source}:5:17-40: cannot use "opsx-gopls-diagnostic" '
            '(untyped string constant) as int value in variable declaration'
        )
        environment = patch.dict(os.environ, GOPLSCACHE=str(self.cache))
        environment.start()
        self.addCleanup(environment.stop)
        output = contextlib.redirect_stdout(io.StringIO())
        output.__enter__()
        self.addCleanup(output.__exit__, None, None, None)

    def test_accepts_real_diagnostic_and_cleans_up_cache_probe(self):
        with patch.object(preflight, 'run', return_value=self.diagnostic):
            preflight.gopls_check(self.scratch, 10)
        self.assertEqual(list((self.cache / 'gopls').iterdir()), [])

    def test_rejects_permission_errors_despite_successful_command(self):
        output = 'unexpected error reading typerefs from filecache: permission denied\n'
        with patch.object(preflight, 'run', return_value=output + self.diagnostic):
            with self.assertRaisesRegex(ValueError, 'permission denied'):
                preflight.gopls_check(self.scratch, 10)

    def test_rejects_missing_diagnostic_despite_successful_command(self):
        with patch.object(preflight, 'run', return_value=''):
            with self.assertRaisesRegex(ValueError, 'expected Go diagnostic'):
                preflight.gopls_check(self.scratch, 10)

    def test_rejects_cache_access_failure_before_running_gopls(self):
        with patch.object(preflight.tempfile, 'NamedTemporaryFile', side_effect=PermissionError), \
                patch.object(preflight, 'run') as run:
            with self.assertRaises(PermissionError):
                preflight.gopls_check(self.scratch, 10)
            run.assert_not_called()


if __name__ == '__main__':
    unittest.main()
