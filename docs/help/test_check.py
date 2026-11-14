"""Setting macro parsing keeps commas inside generics, brackets and strings."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('help_check', Path(__file__).with_name('check.py'))
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


class MacroArguments(unittest.TestCase):
    def test_default_with_turbofish_generics_stays_one_field(self):
        source = 'a, "x, y", serde_json::Map::<String, Value>::new(), NextSafeStep, Configurable) tail'
        self.assertEqual(check.macro_arguments(source, 0), [
            'a', '"x, y"', 'serde_json::Map::<String, Value>::new()', 'NextSafeStep', 'Configurable'])

    def test_arrows_and_brackets_do_not_swallow_fields(self):
        source = 'f::<A, B>(|x| -> u8 { 1 }), vec![1, 2], next) tail'
        self.assertEqual(check.macro_arguments(source, 0), ['f::<A, B>(|x| -> u8 { 1 })', 'vec![1, 2]', 'next'])

    def test_tools_overrides_renders_whole_default_and_adoption(self):
        rows = {row[0]: row for row in check.setting_rows()}
        row = rows['tools.overrides']
        self.assertEqual(row[5], 'serde_json::Map::<String, Value>::new()')
        self.assertEqual(row[6], 'NextSafeStep')
        self.assertEqual(row[7], 'Configurable')


if __name__ == '__main__':
    unittest.main()
