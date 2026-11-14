"""Presidio adapter for obleth's GenAI export NER contract (English only).

POST / with {"texts": [...]} returns {"texts": [...]} of the same length, with
PII replaced by Presidio placeholders.
"""

from fastapi import FastAPI
from pydantic import BaseModel
from presidio_analyzer import AnalyzerEngine
from presidio_anonymizer import AnonymizerEngine

app = FastAPI()
analyzer = AnalyzerEngine()
anonymizer = AnonymizerEngine()


class Texts(BaseModel):
    texts: list[str]


def anonymize(text: str) -> str:
    found = analyzer.analyze(text=text, language="en")
    return anonymizer.anonymize(text=text, analyzer_results=found).text


@app.post("/")
def scrub(body: Texts) -> Texts:
    return Texts(texts=[anonymize(t) for t in body.texts])
