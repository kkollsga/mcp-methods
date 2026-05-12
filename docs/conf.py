# Configuration file for the Sphinx documentation builder.
# https://www.sphinx-doc.org/en/master/usage/configuration.html

project = "mcp-methods"
copyright = "2026, Kristian Kollsgård"
author = "Kristian Kollsgård"

extensions = [
    "myst_parser",
    "sphinx.ext.napoleon",
    "sphinx_copybutton",
]

# -- MyST (Markdown) settings ------------------------------------------------

myst_enable_extensions = [
    "colon_fence",
    "deflist",
    "fieldlist",
]

# Don't fail the build on cross-reference warnings for now — many of
# our internal links cross-reference rustdoc on docs.rs, which Sphinx
# can't resolve.
suppress_warnings = ["myst.xref_missing"]

# -- General settings ---------------------------------------------------------

exclude_patterns = ["_build", "Thumbs.db", ".DS_Store"]
source_suffix = {
    ".rst": "restructuredtext",
    ".md": "markdown",
}

# -- HTML output --------------------------------------------------------------

html_theme = "furo"
html_title = "mcp-methods"
html_theme_options = {
    "source_repository": "https://github.com/kkollsga/mcp-methods",
    "source_branch": "main",
    "source_directory": "docs/",
}
