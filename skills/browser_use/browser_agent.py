import asyncio
import sys
import os
from browser_use import Agent
from langchain_google_genai import ChatGoogleGenerativeAI

async def main():
    if len(sys.argv) < 2:
        print("Usage: python browser_agent.py <task>")
        sys.exit(1)

    task = sys.argv[1]
    
    # Use Gemini API Key from environment
    api_key = os.environ.get("GEMINI_API_KEY")
    if not api_key:
        print("Error: GEMINI_API_KEY not found in environment")
        sys.exit(1)

    llm = ChatGoogleGenerativeAI(model="gemini-2.0-flash", google_api_key=api_key)
    
    class SimpleLLM:
        def __init__(self, llm):
            self.llm = llm
            self.provider = 'google'
            self.model_name = getattr(llm, 'model', 'gemini-2.0-flash')
            self.metadata = getattr(llm, 'metadata', {})
        
        def __getattr__(self, name):
            return getattr(self.llm, name)
            
        async def ainvoke(self, *args, **kwargs):
            return await self.llm.ainvoke(*args, **kwargs)
            
        def invoke(self, *args, **kwargs):
            return self.llm.invoke(*args, **kwargs)

    agent = Agent(
        task=task,
        llm=SimpleLLM(llm),
    )
    
    result = await agent.run()
    print(result)

if __name__ == "__main__":
    asyncio.run(main())
