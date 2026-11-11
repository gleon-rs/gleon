### ❌ gleon: visual regression failure ({{ total_failed }} diffs)

| Test Name |{% if has_baselines %} Baseline |{% endif %} Result |
| :--- |{% if has_baselines %} :---: |{% endif %} :--- |
{% for row in rows -%}
| `{{ row.name }}` |{% if has_baselines %} {{ row.baseline or "N/A" }} |{% endif %} {{ row.result }} |
{% endfor %}
{% if remaining > 0 %}
> ⚠️ **Truncated {{ remaining }} additional diffs.** {% if html_artifact_url -%}
Download the full [gleon HTML report]({{ html_artifact_url }}) to inspect.
{%- else -%}
Render all of them with `gleon report html --out report.html` to inspect them.
{%- endif %}
{% endif -%}
{{ footer }}
