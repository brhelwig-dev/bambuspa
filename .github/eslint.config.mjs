import globals from "globals";

// Only rules that catch outright mistakes, not style.
const rules = {
  "no-undef": "error",
  "no-const-assign": "error",
  "no-dupe-args": "error",
  "no-dupe-else-if": "error",
  "no-dupe-keys": "error",
  "no-func-assign": "error",
  "no-redeclare": "error",
  "no-self-assign": "error",
  "no-sparse-arrays": "error",
  "no-unreachable": "error",
  "no-unsafe-negation": "error",
  "use-isnan": "error",
  "valid-typeof": "error",
};

export default [
  {
    files: ["inline-*.js", "sw.js"],
    languageOptions: {
      ecmaVersion: 2023,
      sourceType: "script",
      globals: { ...globals.browser, ...globals.serviceworker },
    },
    rules,
  },
  {
    files: ["capture.mjs", "compare.mjs"],
    languageOptions: {
      ecmaVersion: 2023,
      sourceType: "module",
      globals: globals.node,
    },
    rules,
  },
];
