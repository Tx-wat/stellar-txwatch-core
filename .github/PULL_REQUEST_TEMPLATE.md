## Summary

<!-- What does this PR do? -->

## Type of change

- [ ] Bug fix
- [ ] New feature (alert rule, CLI command, config option)
- [ ] Performance improvement
- [ ] Documentation
- [ ] Refactoring (no behavior change)

## PR Checklist

- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy -- -D warnings` passes
- [ ] `cargo fmt` applied
- [ ] New rule documented in docs/alert-rules.md (if applicable)
- [ ] CHANGELOG.md entry added under [Unreleased]

## Adding a new alert rule

<!-- Complete this section only when adding a new AlertRule variant. See CONTRIBUTING.md for the full guide. -->

- [ ] Added the variant to the config enum (e.g. `AlertRuleConfig`)
- [ ] Added the matching arm in `eval_rule`
- [ ] Implemented `label()` for the new rule
- [ ] Implemented `rule_type()` for the new rule
- [ ] Added config validation for the new rule's fields
- [ ] Added unit tests covering evaluation, `label()`, and `rule_type()`
- [ ] Added an integration test exercising the rule end-to-end
- [ ] Referenced an existing rule (e.g. a threshold or webhook rule) as the implementation example
