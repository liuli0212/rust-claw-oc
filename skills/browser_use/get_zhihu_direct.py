import asyncio
from playwright.async_api import async_playwright

async def main():
    async with async_playwright() as p:
        browser = await p.chromium.launch(headless=True)
        page = await browser.new_page()
        
        # Set a common user agent to avoid some basic bot detection
        await page.set_extra_http_headers({
            "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/119.0.0.0 Safari/537.36"
        })
        
        url = "https://www.zhihu.com/question/2024270814233511795/answer/2026011770385364009"
        print(f"Navigating to {url}...")
        
        try:
            await page.goto(url, wait_until="networkidle", timeout=60000)
            
            # Wait for the answer content to be visible
            # Zhihu answer content usually has class 'RichText' or 'AnswerItem-content'
            await page.wait_for_selector(".RichText", timeout=10000)
            
            # Extract the title and the answer content
            title = await page.title()
            content = await page.inner_text(".RichText")
            
            print("--- TITLE ---")
            print(title)
            print("--- CONTENT ---")
            print(content)
            
        except Exception as e:
            print(f"Error: {e}")
            # Take a screenshot for debugging if it fails
            await page.screenshot(path="zhihu_error.png")
            print("Screenshot saved to zhihu_error.png")
            
            # Try to get the body text anyway
            body_text = await page.inner_text("body")
            print("--- BODY TEXT ---")
            print(body_text[:1000])
            
        await browser.close()

if __name__ == "__main__":
    asyncio.run(main())
