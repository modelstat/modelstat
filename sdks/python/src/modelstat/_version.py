"""Single source of truth for the package version.

Read both at runtime (to build ``Config.client_version`` -> the wire
``daemon_version``) and by hatchling at build time (see ``pyproject.toml``'s
``[tool.hatch.version]``), so the two can never drift.
"""

__version__ = "0.0.2"
