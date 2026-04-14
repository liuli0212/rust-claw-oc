import asyncio
from browser_use import Agent
from langchain_openai import ChatOpenAI
import os

class LLMWrapper:
    def __init__(self, llm):
        self.llm = llm
        self.provider = 'openai'
    
    def __getattr__(self, name):
        return getattr(self.llm, name)
    
    async def ainvoke(self, *args, **kwargs):
        return await self.llm.ainvoke(*args, **kwargs)

async def main():
    task = "Go to https://www.zhihu.com/question/2024270814233511795/answer/2026011770385364009 and extract the full text of the answer. Ignore comments and sidebars. Just give me the main content."
    
    base_llm = ChatOpenAI(
        model="gemini-2.0-flash-exp",
        api_key=os.environ.get("GEMINI_API_KEY"),
        base_url="https://generativelanguage.googleapis.com/v1beta/openai/"
    )
    
    llm = LLMWrapper(base_llm)
    
    agent = Agent(
        task=task,
        llm=llm,
    )
    result = await agent.run()
    print(result.final_result())

if __name__ == "__main__":
    asyncio.run(main())
