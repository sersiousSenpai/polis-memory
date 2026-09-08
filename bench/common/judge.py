"""The LongMemEval judge: one fixed judge model, the question-type-specific
yes/no templates adapted from the LongMemEval evaluation script
(`evaluate_qa.py`). Verify against the upstream text before a keyed run and
keep this file byte-stable across the runs a table compares.
"""
from __future__ import annotations

from .llm import Llm

JUDGE_SYSTEM = "You are a strict grader. Answer with exactly one word: yes or no."

_TEMPLATES = {
    "default": (
        "I will give you a question, a correct answer, and a response from a model. "
        "Please answer yes if the response contains the correct answer. Otherwise, answer no. "
        "If the response is equivalent to the correct answer or contains all the intermediate steps to get the correct answer, "
        "you should also answer yes. If the response only contains a subset of the information required by the answer, answer no."
    ),
    "temporal-reasoning": (
        "I will give you a question, a correct answer, and a response from a model. "
        "Please answer yes if the response contains the correct answer. Otherwise, answer no. "
        "If the response is equivalent to the correct answer or contains all the intermediate steps to get the correct answer, "
        "you should also answer yes. If the response only contains a subset of the information required by the answer, answer no. "
        "In addition, do not penalize off-by-one errors for the number of days. If the question asks for the number of days/weeks/months, "
        "etc., and the model makes off-by-one errors (e.g., predicting 19 days when the answer is 18), the model's response is still correct."
    ),
    "knowledge-update": (
        "I will give you a question, a correct answer, and a response from a model. "
        "Please answer yes if the response contains the correct answer. Otherwise, answer no. "
        "If the response contains some previous information along with an updated answer, the response should be considered as correct "
        "as long as the updated answer is the required answer."
    ),
    "single-session-preference": (
        "I will give you a question, a rubric for desired personalized response, and a response from a model. "
        "Please answer yes if the response satisfies the desired response. Otherwise, answer no. "
        "The model does not need to reflect all the points in the rubric. The response is correct as long as it recalls and utilizes "
        "the user's personal information correctly."
    ),
    "abstention": (
        "I will give you an unanswerable question, an explanation, and a response from a model. "
        "Please answer yes if the model correctly identifies the question as unanswerable. The model could say that the information "
        "is incomplete, or some other information is given but the asked information is not. Otherwise, answer no."
    ),
}


def template_for(qtype: str, question_id: str) -> str:
    if question_id.endswith("_abs"):
        return _TEMPLATES["abstention"]
    return _TEMPLATES.get(qtype, _TEMPLATES["default"])


def judge(llm: Llm, qtype: str, question_id: str, question: str, gold: str, response: str) -> bool:
    prompt = (
        f"{template_for(qtype, question_id)}\n\n"
        f"Question: {question}\n\n<gold>{gold}</gold>\n\n<response>{response}</response>\n\n"
        "Is the model response correct? Answer yes or no only."
    )
    verdict = llm.complete("judge", JUDGE_SYSTEM, prompt, max_tokens=4).strip().lower()
    return verdict.startswith("yes")
