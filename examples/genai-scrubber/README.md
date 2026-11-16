# Presidio scrubber for obleth

Adapter between obleth's NER contract and Presidio. Used by `OBLETH_PII_NER_URL`
(GenAI trace scrubbing and guardrails PII profiles with `detector = ner`).

Files:

- `app.py`: the FastAPI service.
- `Dockerfile`, `requirements.txt`: image build.

## Contract

```json
POST /
{"texts": ["..."], "entities": ["PERSON"], "language": "en", "score_threshold": 0.5}
-> {"texts": ["..."]}
```

Only `texts` is required. Absent or null: `language` is `en`, `entities` is every
entity Presidio knows, `score_threshold` is Presidio's default. The reply has the
same length as `texts`.

## Other languages

The adapter runs Presidio's default setup: English, with Presidio's predefined
recognizers. For another language, load its spaCy model through Presidio's
`NlpEngineProvider`, register recognizers for that language, and send `language`
in the request (a profile's `language` field). Country-specific identifiers
are custom `PatternRecognizer`s added to the registry.
