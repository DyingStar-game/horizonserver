# 🧪 Running Tests

## Prerequisites

Before running the tests, make sure you have the following installed:

- **Node.js** (version 16 or higher recommended)
- **npm** or **yarn**
- **TypeScript** and **ts-node** (installed as project dependencies)
- **Mocha** and **Chai** (installed as dev dependencies)
- **Zod** — for runtime schema validation and type-safe data testing

You can install all dependencies by running:

```bash
npm install
```

---

## Test Framework

This project uses:

- [**Mocha**](https://mochajs.org/) — test runner
- [**Chai**](https://www.chaijs.com/) — assertion library
- [**ts-node**](https://typestrong.org/ts-node/) — to execute TypeScript files without compiling them manually
- [**zod**](https://zod.dev/) — for schema validation during tests

---

## Running All Tests

To execute all test files under the `test/` directory, run:

```bash
npm test
```

---

## Running a Specific Test File

To run a single test file:

```bash
npm test -- --grep "<test title>"
```

Example:

```bash
npm test -- --grep "build a ws player message login with ddurieux login"
```

---

## Running a Single Test Case

You can isolate a single test or test suite by adding `.only` in your test file:

```ts
describe.only("UserService", () => {
  it("should create a user", () => {
    // your test
  });
});
```

or on a specific test:

```ts
it.only("should return the correct result", () => {
  // your test
});
```

Mocha will run only the marked test(s).

---

## Running Tests Matching a Pattern

You can also run tests that match a specific title using the `--grep` option:

```bash
npm test -- --grep "should create a user"
```

---

## Running Tests with specific seed (Faker)

You can also run tests with a specific seed using the `FAKER_SEED` env:

```bash
FAKER_SEED=1234 npm test
```

---
