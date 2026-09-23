// This runs in an isolated script world. Email scripts and network loads are disabled.
(() => {
    function clean(html) {
        const doc = new DOMParser().parseFromString(html, 'text/html');
        doc.querySelectorAll('script,iframe,frame,frameset,object,embed,applet,base,meta,link,form,input,button,textarea,select,svg,math').forEach(node => node.remove());
        doc.querySelectorAll('*').forEach(node => {
            for (const attr of [...node.attributes]) {
                if (/^on/i.test(attr.name) || ['contenteditable', 'autofocus', 'srcdoc', 'srcset'].includes(attr.name) ||
                    (['href', 'src', 'action', 'background', 'xlink:href'].includes(attr.name) && !/^(https?:|mailto:|cid:|data:image\/(png|jpeg|gif|webp);base64,|#)/i.test(attr.value.trim()))) {
                    node.removeAttribute(attr.name);
                }
            }
        });
        // Preserve full-document email styles when embedding it as a fragment.
        const wrapper = doc.createElement('div');
        for (const name of ['style', 'class', 'id', 'dir', 'lang']) {
            if (doc.body.hasAttribute(name)) wrapper.setAttribute(name, doc.body.getAttribute(name));
        }
        if (doc.body.hasAttribute('bgcolor')) wrapper.style.backgroundColor = doc.body.getAttribute('bgcolor');
        if (doc.body.hasAttribute('text')) wrapper.style.color = doc.body.getAttribute('text');
        wrapper.innerHTML = doc.body.innerHTML;
        return [...doc.head.querySelectorAll('style')].map(node => node.outerHTML).join('') +
            (wrapper.hasAttributes() ? wrapper.outerHTML : wrapper.innerHTML);
    }
    document.body.innerHTML = clean(INITIAL_HTML);
    const images = INITIAL_IMAGES;
    for (const img of document.images) {
        const src = img.getAttribute('src') || '';
        if (/^cid:/i.test(src)) {
            const image = images.find(image => image.cid.toLowerCase() === src.slice(4).toLowerCase());
            if (image) img.setAttribute('src', image.src);
        }
    }
    document.body.contentEditable = 'true';
    document.body.setAttribute('role', 'textbox');
    document.body.setAttribute('aria-label', 'Message body');
    document.body.setAttribute('aria-multiline', 'true');
    let pending = 0;
    let pasteError = null;
    let pasteId = 0;
    const pasteRanges = new Map();
    document.addEventListener('click', event => {
        if (event.target.closest('a')) event.preventDefault();
    });
    document.addEventListener('drop', event => event.preventDefault());
    document.addEventListener('paste', event => {
        const data = event.clipboardData;
        if (!data) return;
        const files = [...data.items].filter(item => item.kind === 'file' && /^image\/(png|jpeg|gif|webp)$/.test(item.type));
        const html = data.getData('text/html');
        if (files.length) {
            event.preventDefault();
            const range = getSelection().rangeCount ? getSelection().getRangeAt(0).cloneRange() : null;
            pending++;
            Promise.all(files.map(item => new Promise((resolve, reject) => {
                const reader = new FileReader();
                reader.onload = () => resolve(reader.result);
                reader.onerror = () => reject(new Error('Could not paste image'));
                reader.readAsDataURL(item.getAsFile());
            }))).then(sources => {
                if (range) { getSelection().removeAllRanges(); getSelection().addRange(range); }
                document.execCommand('insertHTML', false, sources.map(src => `<img src="${src}" style="max-width:100%;height:auto">`).join(''));
            }).catch(error => { pasteError = error.message; }).finally(() => pending--);
        } else if (html) {
            event.preventDefault();
            document.execCommand('insertHTML', false, clean(html));
        }
    });
    const range = document.createRange();
    range.selectNodeContents(document.body);
    range.collapse(true);
    getSelection().removeAllRanges();
    getSelection().addRange(range);
    globalThis.postbirdEditor = {
        clean,
        beginPaste() {
            const id = ++pasteId;
            pasteRanges.set(id, getSelection().rangeCount ? getSelection().getRangeAt(0).cloneRange() : null);
            pending++;
            return id;
        },
        finishPaste(id, html) {
            const range = pasteRanges.get(id);
            pasteRanges.delete(id);
            pending--;
            if (html !== null) {
                document.body.focus();
                if (range) { getSelection().removeAllRanges(); getSelection().addRange(range); }
                document.execCommand('insertHTML', false, clean(html));
            }
        },
        snapshot() {
            if (pending) throw new Error('Wait for the pasted image to finish loading');
            if (pasteError) { const error = pasteError; pasteError = null; throw new Error(error); }
            const body = document.body.cloneNode(true);
            for (const img of body.querySelectorAll('img')) {
                const image = images.find(image => image.src === img.getAttribute('src'));
                if (image) img.setAttribute('src', 'cid:' + image.cid);
            }
            return JSON.stringify({html: clean(body.innerHTML), plain: document.body.innerText});
        }
    };
    true;
})()
