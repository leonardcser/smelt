import json
import statistics
import unittest
from pathlib import Path

import charts


class ChartsTests(unittest.TestCase):
    def test_readme_matches_latest_results(self):
        folder = max((Path(__file__).parent / "results").iterdir())
        results = {
            mode: json.loads((folder / f"{mode}.json").read_text())
            for mode in charts.KEY
        }
        table = charts.README.read_text().split("<!-- bench:table -->\n", 1)[1]
        table = table.split("\n<!-- /bench -->", 1)[0]
        self.assertEqual(charts.readme_table(table, results), table)
        rows = {
            cells[0].strip("*"): cells
            for line in table.splitlines()
            if line.startswith("|")
            and (cells := [cell.strip() for cell in line.split("|")[1:-1]])
        }
        for mode, key in charts.KEY.items():
            column = 2 if mode == "cold-start" else 3
            for agent, runs in results[mode]["agents"].items():
                row = rows[charts.NAMES.get(agent, agent)]
                self.assertEqual(row[1], f"`{results[mode]['versions'][agent]}`")
                median = statistics.median(run[key] for run in runs) / charts.MIB
                self.assertEqual(row[column].strip("*"), f"{median:.0f} MiB")


if __name__ == "__main__":
    unittest.main()
