"""Presidio adapter for obleth's NER contract.

POST / with {"texts": [...], "entities": [...], "language": "en",
"score_threshold": 0.5} returns {"texts": [...]} of the same length, with PII
replaced by Presidio placeholders. Only "texts" is required.

Uses Presidio's default setup (English). See README.md for other languages.
"""

from fastapi import FastAPI
from pydantic import BaseModel
from presidio_analyzer import AnalyzerEngine
from presidio_anonymizer import AnonymizerEngine

app = FastAPI()


analyzer = AnalyzerEngine()
anonymizer = AnonymizerEngine()


class Request(BaseModel):
    texts: list[str]
    entities: list[str] | None = None
    language: str | None = None
    score_threshold: float | None = None


class Texts(BaseModel):
    texts: list[str]


def anonymize(
    text: str, language: str, entities: list[str] | None, threshold: float | None
) -> str:
    kwargs = {} if threshold is None else {"score_threshold": threshold}
    found = analyzer.analyze(
        text=text, language=language, entities=entities or None, **kwargs
    )
    return anonymizer.anonymize(text=text, analyzer_results=found).text


@app.post("/")
def scrub(body: Request) -> Texts:
    language = body.language or "en"
    return Texts(
        texts=[
            anonymize(t, language, body.entities, body.score_threshold)
            for t in body.texts
        ]
    )
