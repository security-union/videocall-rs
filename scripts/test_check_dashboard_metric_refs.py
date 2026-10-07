#!/usr/bin/env python3
"""Regression tests for check_dashboard_metric_refs.py (#2922).

Each test builds a throwaway tree, breaks one property the guard claims, and
asserts the guard notices. The last test runs it against the real tree.
"""
from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().with_name("check_dashboard_metric_refs.py")
ROOT = SCRIPT.parent.parent

METRICS_RS = textwrap.dedent(
    """
    lazy_static! {
        pub static ref LIVE: GaugeVec = register_gauge_vec!(
            "videocall_live_gauge",
            "help (with parens)",
            &["meeting_id", "session_id"]
        )
        .expect("x");
        pub static ref LAT: Histogram = register_histogram!(
            "relay_live_latency_ms",
            "help",
            vec![1.0]
        )
        .expect("x");
    }
    const NOT_A_METRIC: &str = "videocall_only_a_string";
    fn collector() -> Desc {
        Desc::new("relay_collected_total".into(), "help".into(), vec!["room_id".into()], HashMap::new())
            .expect("x")
    }
    """
)


def rules(expr_block):
    return "groups:\n- name: videocall_group_name\n  rules:\n" + textwrap.indent(
        expr_block, "  "
    )


def dashboard(*exprs):
    return json.dumps(
        {
            "panels": [{"targets": [{"expr": e} for e in exprs]}],
            "templating": {
                "list": [{"query": {"query": "label_values(videocall_live_gauge, meeting_id)"}}]
            },
        }
    )


class Guard(unittest.TestCase):
    def tree(self, rule_text=None, dash_text=None, values_text=None):
        tmp = Path(tempfile.mkdtemp(prefix="metric-refs-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        (tmp / "crate/src").mkdir(parents=True)
        (tmp / "crate/src/metrics.rs").write_text(METRICS_RS)
        prom = tmp / "docker/monitoring/prometheus"
        prom.mkdir(parents=True)
        (prom / "alert_rules.yml").write_text(
            rule_text or rules("- alert: A\n  expr: videocall_live_gauge > 1\n")
        )
        dash = tmp / "helm/grafana/dashboards"
        dash.mkdir(parents=True)
        (dash / "d.json").write_text(dash_text or dashboard("videocall_live_gauge"))
        if values_text:
            vals = tmp / "helm/global/c1/prometheus"
            vals.mkdir(parents=True)
            (vals / "values.yaml").write_text(values_text)
        return tmp

    def run_guard(self, root):
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--root", str(root)],
            capture_output=True,
            text=True,
        )

    def assert_flags(self, root, name):
        res = self.run_guard(root)
        self.assertEqual(res.returncode, 1, res.stdout + res.stderr)
        self.assertIn(name, res.stdout)

    def test_a_clean_tree_passes(self):
        expr = (
            'histogram_quantile(0.99, rate(relay_live_latency_ms_bucket{transport="relay_ws"}[5m]))'
            " + sum by (meeting_id) (videocall_live_gauge{session_id=~\"$session_id\"})"
        )
        root = self.tree(
            rule_text=rules("- alert: A\n  expr: |\n    " + expr + "\n"),
            dash_text=dashboard(expr),
        )
        res = self.run_guard(root)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)

    def test_a_dead_single_line_alert_expr_fails(self):
        root = self.tree(
            rule_text=rules("- alert: A\n  expr: videocall_gone_total > 0\n")
        )
        self.assert_flags(root, "videocall_gone_total")

    def test_a_dead_quoted_single_line_alert_expr_fails(self):
        for quoted in ('"videocall_gone_total > 0"', "'videocall_gone_total > 0'"):
            with self.subTest(quoted=quoted):
                root = self.tree(rule_text=rules(f"- alert: A\n  expr: {quoted}\n"))
                self.assert_flags(root, "videocall_gone_total")

    def test_a_dead_name_on_a_continuation_line_of_a_block_expr_fails(self):
        root = self.tree(
            rule_text=rules(
                "- alert: A\n  expr: |\n    videocall_live_gauge\n      and videocall_gone_total\n  for: 1m\n"
            )
        )
        self.assert_flags(root, "videocall_gone_total")

    def test_a_dead_dashboard_target_fails(self):
        root = self.tree(dash_text=dashboard("videocall_live_gauge", "videocall_gone_total"))
        self.assert_flags(root, "videocall_gone_total")

    def test_a_dead_name_in_a_chart_values_rule_fails(self):
        values = textwrap.dedent(
            """
            serverFiles:
              alerting_rules.yml:
                groups:
                  - name: g
                    rules:
                      - alert: A
                        expr: rate(relay_gone_total[1m]) > 0
            """
        )
        root = self.tree(values_text=values)
        self.assert_flags(root, "relay_gone_total")

    def test_a_string_literal_is_not_a_registration(self):
        root = self.tree(dash_text=dashboard("videocall_only_a_string"))
        self.assert_flags(root, "videocall_only_a_string")

    def test_a_collector_desc_counts_as_a_registration(self):
        root = self.tree(dash_text=dashboard('sum by (room_id) (relay_collected_total)'))
        res = self.run_guard(root)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)

    def test_a_collector_table_with_computed_desc_names_counts_as_registration(self):
        # The shape #2925's NatsStatisticsCollector uses.
        collector = textwrap.dedent(
            """
            type Reader = fn(&async_nats::Statistics) -> u64;
            const NATS_STATISTICS_COUNTERS: [(&str, &str, Reader); 2] = [
                (
                    "relay_nats_in_messages_total",
                    "Messages received [sic] (#2925)",
                    |s| s.in_messages.load(std::sync::atomic::Ordering::Relaxed),
                ),
                (
                    "relay_nats_connects_total",
                    "Connections",
                    |s| s.connects.load(std::sync::atomic::Ordering::Relaxed),
                ),
            ];
            fn descs() -> Vec<Desc> {
                NATS_STATISTICS_COUNTERS
                    .iter()
                    .map(|(name, help, _)| {
                        prometheus::core::Desc::new(
                            name.to_string(),
                            help.to_string(),
                            Vec::new(),
                            std::collections::HashMap::new(),
                        )
                    })
                    .collect()
            }
            """
        )
        root = self.tree(
            dash_text=dashboard(
                "rate(relay_nats_in_messages_total[5m])", "relay_nats_connects_total"
            )
        )
        (root / "crate/src/collector.rs").write_text(collector)
        res = self.run_guard(root)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)

    def test_a_str_tuple_table_without_a_collector_is_not_a_registration(self):
        root = self.tree(dash_text=dashboard("relay_nats_in_messages_total"))
        (root / "crate/src/table.rs").write_text(
            'const T: [(&str, u32); 1] = [("relay_nats_in_messages_total", 1)];\n'
        )
        self.assert_flags(root, "relay_nats_in_messages_total")

    def test_a_root_without_rust_is_a_usage_error(self):
        root = Path(tempfile.mkdtemp(prefix="metric-refs-empty-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        self.assertEqual(self.run_guard(root).returncode, 2)

    def test_a_root_without_rule_or_dashboard_files_is_a_usage_error(self):
        root = self.tree()
        shutil.rmtree(root / "docker")
        shutil.rmtree(root / "helm")
        self.assertEqual(self.run_guard(root).returncode, 2)

    def test_the_real_tree_passes(self):
        res = self.run_guard(ROOT)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)


if __name__ == "__main__":
    unittest.main()
