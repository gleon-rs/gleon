### ❌ Gleon Visual Regression Failure ({{ total_failed }} diffs)

| Test Name |{% if has_baselines %} Baseline |{% endif %} Result |
| :--- |{% if has_baselines %} :---: |{% endif %} :--- |
{% for row in rows -%}
| `{{ row.name }}` |{% if has_baselines %} {{ row.baseline or "N/A" }} |{% endif %} {{ row.result }} |
{% endfor %}
{% if remaining > 0 %}
> ⚠️ **Truncated {{ remaining }} additional diffs.** {% if html_artifact_url -%}
Download the full [Gleon HTML Report]({{ html_artifact_url }}) to inspect.
{%- else -%}
Download the full HTML Report from GitHub Action Artifacts to inspect.
{%- endif %}
{% endif -%}
{{ footer }}
