"""Render the approved latest-model planning data as an offline reader."""
from html import escape
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT/'docs/planning'
plan = json.loads((OUT/'latest-model-plan.json').read_text())
assert plan['status'].startswith('Approved')
implementation = plan['implementation']
latest = implementation['new_item']
esc = escape
parts = ['<!doctype html><html lang="en"><meta charset="utf-8">',
    '<meta name="viewport" content="width=device-width,initial-scale=1">',
    '<link rel="icon" href="data:,"><title>Latest model and availability plan</title>',
    '<style>body{max-width:1000px;margin:40px auto;padding:0 24px;background:#faf9f6;color:#243033;font:16px/1.65 system-ui,sans-serif}h1,h2,h3{line-height:1.25}h1{font-size:34px}h2{margin-top:36px}a{color:#245b8a}table{width:100%;border-collapse:collapse;background:white}th,td{border:1px solid #d5d8d4;padding:12px;text-align:left;vertical-align:top}th{background:#e9ece6}code{overflow-wrap:anywhere}details{border:1px solid #d5d8d4;padding:12px;margin:12px 0;background:white}summary{cursor:pointer;font-weight:600}.notice{padding:16px;border-left:4px solid #6b7d62;background:#eef1e9}li{margin:8px 0}@media(max-width:650px){body{padding:0 14px}th,td{padding:8px}h1{font-size:28px}}</style>',
    '<h1>Latest model and availability plan</h1>',
    '<p class="notice">Approved 5 November 2026 under DW-F6. Native implementation and acceptance remain pending. CodeGraph is the operational source of truth.</p>',
    f'<p>{esc(plan["objective"])}</p>',
    '<p><a href="auto-routing-plan.html">Approved Auto predecessor</a> · <a href="../architecture/implementation-plan.html#dw-108">Delivery slice</a> · <a href="../architecture/devworks-architecture.html#latest-model-aliases-and-catalog-availability">Architecture</a> · <a href="latest-model-plan.json">Full plan data</a></p>',
    '<h2 id="aliases">Family aliases and tiers</h2><table><tr><th>Tier</th><th>OpenAI</th><th>Anthropic</th></tr>']
for row in plan['tier_aliases']:
    parts.append('<tr>'+''.join('<td>'+esc(v)+'</td>' for v in row)+'</tr>')
parts.append('</table><h2 id="requirements">Confirmed requirements</h2>')
for delta in plan['requirements_delta']:
    parts.append(f'<details open id="{esc(delta["source_requirement"])}"><summary>{esc(delta["source_requirement"])} · {esc(delta["feature_requirement"])}</summary><p>{esc(delta["after"])}</p></details>')
parts.append('<h2 id="items">Implementation and acceptance</h2>')
parts.append(f'<h3>{esc(latest["key"])} {esc(latest["title"])}</h3><p>{esc(latest["goal"])}</p>')
for a in latest['acceptance_criteria']:
    parts.append(f'<p><strong>{esc(a["id"])}</strong> {esc(a["text"])}</p>')
parts.append(f'<details><summary>Implementation and validation notes</summary><p>{esc(latest["description"]).replace(chr(10),"<br>")}</p></details>')
for change in implementation['existing_item_changes']:
    key = 'DW-107' if change['ref']=='session-auto-routing' else change['ref']
    parts.append(f'<h3>{esc(key)} amendment</h3>')
    for a in change['acceptance_criteria']:
        parts.append(f'<details><summary>{esc(a["id"])}</summary><p>{esc(a["text"])}</p></details>')
parts.append('<h3>DW-61 final acceptance</h3>')
parts.append('<p>'+esc(implementation['final_acceptance_change']['acceptance_criteria'][-1]['text'])+'</p>')
parts.append('<h2 id="dependencies">Dependencies and cursor boundaries</h2><ul>')
parts.extend('<li>'+esc(note)+'</li>' for note in plan['dependency_notes'])
parts.append('</ul><h2 id="verification">Evidence boundary</h2><p>'+esc(plan['validation'])+'</p>')
parts.append('<p>Reuse native evidence across DW-108, DW-22, DW-107 and DW-61. This document does not run or certify inference, credentials, provider compatibility or native UI behavior.</p></html>')
(OUT/'latest-model-plan.html').write_text('\n'.join(parts)+'\n')
print('Rendered latest-model-plan.html')
