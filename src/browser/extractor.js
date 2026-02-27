// OpenClaw Browser Extractor
// Injected into the page to extract interactive elements and build a flattened reference map.

(function() {
    window.__OC_NODE_MAP__ = window.__OC_NODE_MAP__ || {};
    window.__OC_NEXT_ID__ = window.__OC_NEXT_ID__ || 1;

    function isVisible(el) {
        if (!el || el.nodeType !== Node.ELEMENT_NODE) return false;
        const style = window.getComputedStyle(el);
        if (style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0') {
            return false;
        }
        const rect = el.getBoundingClientRect();
        return rect.width > 0 && rect.height > 0;
    }

    function isInteractive(el) {
        const tagName = el.tagName.toLowerCase();
        if (['a', 'button', 'input', 'select', 'textarea'].includes(tagName)) return true;
        if (el.hasAttribute('onclick') || el.getAttribute('role') === 'button') return true;
        if (el.isContentEditable) return true;
        return false;
    }

    function getElementText(el) {
        const text = (el.innerText || el.textContent || '').trim();
        return text.replace(/\s+/g, ' ').substring(0, 100);
    }

    const elements = [];
    const root = document.body;
    
    // Process elements
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, null, false);
    let currentNode;
    while ((currentNode = walker.nextNode())) {
        if (!isVisible(currentNode)) continue;
        
        // Simple culling: roughly check if in viewport
        const rect = currentNode.getBoundingClientRect();
        const inViewport = (
            rect.top >= 0 &&
            rect.left >= 0 &&
            rect.bottom <= (window.innerHeight || document.documentElement.clientHeight) &&
            rect.right <= (window.innerWidth || document.documentElement.clientWidth)
        );

        if (!inViewport && !isInteractive(currentNode)) continue;

        if (isInteractive(currentNode)) {
            // Assign ID
            let id = currentNode.getAttribute('data-oc-id');
            if (!id) {
                id = window.__OC_NEXT_ID__++;
                currentNode.setAttribute('data-oc-id', id);
                window.__OC_NODE_MAP__[id] = {
                    tagName: currentNode.tagName,
                    x: rect.left + rect.width / 2,
                    y: rect.top + rect.height / 2
                };
            }

            const tagName = currentNode.tagName.toLowerCase();
            const text = getElementText(currentNode);
            let desc = tagName;
            if (tagName === 'input') {
                desc += `-${currentNode.getAttribute('type') || 'text'}`;
                const placeholder = currentNode.getAttribute('placeholder');
                if (placeholder) desc += ` (placeholder="${placeholder}")`;
            } else {
                desc += ` "${text}"`;
            }
            
            elements.push(`[${id}] ${desc}`);
        }
    }

    return '# Viewport Content\n' + elements.join('\n');
})();
